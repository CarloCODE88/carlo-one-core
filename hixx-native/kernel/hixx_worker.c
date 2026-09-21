#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/fs.h>
#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/ioctl.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/mm.h>
#include <linux/vmalloc.h>
#include <linux/wait.h>
#include <linux/spinlock.h>

#define HIXX_MODULE_NAME "hixx_worker"
#define HIXX_CLASS_NAME "hixx"
#define HIXX_MAX_DEVICES 4
#define HIXX_RINGBUFFER_SIZE (256 * 1024)
#define HIXX_MAX_REQUESTS 64
#define HIXX_IOCTL_MAGIC 'h'

/* IOCTL commands */
#define HIXX_IOCTL_LOAD_MODEL   _IOW(HIXX_IOCTL_MAGIC, 0, struct hixx_model_req)
#define HIXX_IOCTL_RUN_INFERENCE _IOW(HIXX_IOCTL_MAGIC, 1, struct hixx_infer_req)
#define HIXX_IOCTL_GET_METRICS  _IOWR(HIXX_IOCTL_MAGIC, 2, struct hixx_metrics)
#define HIXX_IOCTL_RINGPTR    _IOWR(HIXX_IOCTL_MAGIC, 3, struct hixx_ringptr)

/* Model request structure */
struct hixx_model_req {
    u64 model_size;
    u64 vma_offset;
    u32 gpu_device;
    u8  quantization;
    u8  padding[3];
};

/* Inference request structure */
struct hixx_infer_req {
    u64 input_buffer;
    u64 output_buffer;
    u32 input_size;
    u32 output_size;
    u32 context_tokens;
    u32 max_tokens;
    u32 gpu_device;
};

/* Metrics structure */
struct hixx_metrics {
    u64 gpu_utilization;
    u64 gpu_temperature;
    u64 gpu_clock_mhz;
    u64 memory_used_mb;
    u64 memory_total_mb;
    u64 inference_count;
    u64 total_tokens;
};

/* Ring buffer slot */
struct hixx_slot {
    u64 physical_addr;
    u32 size;
    u32 flags;
    u64 timestamp;
};

/* Ring buffer pointer for user-space access */
struct hixx_ringptr {
    u32 write_index;
    u32 read_index;
    u32 count;
    u64 slots_phys;
};

/* Shared memory ring buffer */
struct hixx_ringbuffer {
    struct hixx_slot slots[HIXX_MAX_REQUESTS];
    u32 write_index;
    u32 read_index;
    u32 count;
    spinlock_t lock;
    wait_queue_head_t wq;
};

/* Per-device state */
struct hixx_device {
    dev_t devno;
    struct cdev cdev;
    struct device *dev;
    void *vaddr;
    struct hixx_ringbuffer ring;
    struct hixx_metrics metrics;
    spinlock_t metrics_lock;
    u64 inference_count;
};

static struct hixx_device *hixx_devices;
static int hixx_major;
static struct class *hixx_class;

/* Ring buffer operations */
static int hixx_ring_push(struct hixx_ringbuffer *ring, struct hixx_slot *slot)
{
    unsigned long flags;
    spin_lock_irqsave(&ring->lock, flags);
    if (ring->count >= HIXX_MAX_REQUESTS) {
        spin_unlock_irqrestore(&ring->lock, flags);
        return -EBUSY;
    }
    ring->slots[ring->write_index] = *slot;
    ring->write_index = (ring->write_index + 1) % HIXX_MAX_REQUESTS;
    ring->count++;
    spin_unlock_irqrestore(&ring->lock, flags);
    wake_up_interruptible(&ring->wq);
    return 0;
}

static int hixx_ring_pop(struct hixx_ringbuffer *ring, struct hixx_slot *slot)
{
    unsigned long flags;
    spin_lock_irqsave(&ring->lock, flags);
    while (ring->count == 0) {
        spin_unlock_irqrestore(&ring->lock, flags);
        if (wait_event_interruptible(ring->wq, ring->count > 0))
            return -ERESTARTSYS;
        spin_lock_irqsave(&ring->lock, flags);
    }
    *slot = ring->slots[ring->read_index];
    ring->read_index = (ring->read_index + 1) % HIXX_MAX_REQUESTS;
    ring->count--;
    spin_unlock_irqrestore(&ring->lock, flags);
    return 0;
}

static long hixx_ioctl(struct file *filp, unsigned int cmd, unsigned long arg)
{
    struct hixx_device *dev = filp->private_data;
    void __user *ubuf = (void __user *)arg;
    int ret = 0;

    switch (cmd) {
    case HIXX_IOCTL_LOAD_MODEL: {
        struct hixx_model_req req;
        if (copy_from_user(&req, ubuf, sizeof(req)))
            return -EFAULT;
        pr_info("hixx: Loading model %llu bytes to GPU %u (q=%u)\n",
                req.model_size, req.gpu_device, req.quantization);
        spin_lock(&dev->metrics_lock);
        dev->metrics.inference_count = 0;
        dev->inference_count = 0;
        dev->metrics.total_tokens = 0;
        spin_unlock(&dev->metrics_lock);
        break;
    }
    case HIXX_IOCTL_RUN_INFERENCE: {
        struct hixx_infer_req req;
        if (copy_from_user(&req, ubuf, sizeof(req)))
            return -EFAULT;
        pr_debug("hixx: Inference: in=%u out=%u ctx=%u max=%u gpu=%u\n",
                req.input_size, req.output_size, req.context_tokens,
                req.max_tokens, req.gpu_device);
        spin_lock(&dev->metrics_lock);
        dev->inference_count++;
        dev->metrics.inference_count = dev->inference_count;
        dev->metrics.total_tokens += req.max_tokens;
        spin_unlock(&dev->metrics_lock);
        break;
    }
    case HIXX_IOCTL_GET_METRICS: {
        struct hixx_metrics metrics = dev->metrics;
        spin_lock(&dev->metrics_lock);
        metrics.gpu_utilization = 0;
        metrics.gpu_temperature = 0;
        spin_unlock(&dev->metrics_lock);
        if (copy_to_user(ubuf, &metrics, sizeof(metrics)))
            return -EFAULT;
        break;
    }
    case HIXX_IOCTL_RINGPTR: {
        struct hixx_ringptr ptr;
        ptr.write_index = dev->ring.write_index;
        ptr.read_index = dev->ring.read_index;
        ptr.count = dev->ring.count;
        ptr.slots_phys = virt_to_phys(dev->ring.slots);
        if (copy_to_user(ubuf, &ptr, sizeof(ptr)))
            return -EFAULT;
        break;
    }
    default:
        pr_warn("hixx: Unknown ioctl 0x%08x\n", cmd);
        return -ENOTTY;
    }
    return 0;
}

static int hixx_open(struct inode *inode, struct file *filp)
{
    struct hixx_device *dev = container_of(inode->i_cdev, struct hixx_device, cdev);
    filp->private_data = dev;
    pr_debug("hixx: Device opened (minor=%d)\n", iminor(inode));
    return 0;
}

static int hixx_release(struct inode *inode, struct file *filp)
{
    pr_debug("hixx: Device released\n");
    return 0;
}

static const struct file_operations hixx_fops = {
    .owner = THIS_MODULE,
    .open = hixx_open,
    .release = hixx_release,
    .unlocked_ioctl = hixx_ioctl,
};

static int __init hixx_init(void)
{
    int i, err;
    dev_t devno;

    pr_info("hixx: Hixx-Server Kernel-Native Worker v2.0 loading...\n");

    err = alloc_chrdev_region(&devno, 0, HIXX_MAX_DEVICES, HIXX_MODULE_NAME);
    if (err < 0) {
        pr_err("hixx: Failed to allocate chrdev region\n");
        return err;
    }
    hixx_major = MAJOR(devno);

    hixx_devices = kcalloc(HIXX_MAX_DEVICES, sizeof(struct hixx_device), GFP_KERNEL);
    if (!hixx_devices) {
        err = -ENOMEM;
        goto fail_devices;
    }

    hixx_class = class_create(THIS_MODULE, HIXX_CLASS_NAME);
    if (IS_ERR(hixx_class)) {
        err = PTR_ERR(hixx_class);
        goto fail_class;
    }

    for (i = 0; i < HIXX_MAX_DEVICES; i++) {
        struct hixx_device *dev = &hixx_devices[i];
        dev->devno = MKDEV(hixx_major, i);
        cdev_init(&dev->cdev, &hixx_fops);
        dev->cdev.owner = THIS_MODULE;
        err = cdev_add(&dev->cdev, dev->devno, 1);
        if (err < 0) {
            pr_err("hixx: Failed to add cdev %d\n", i);
            goto fail_cdev;
        }

        dev->dev = device_create(hixx_class, NULL, dev->devno, NULL, "hixx%d", i);
        if (IS_ERR(dev->dev)) {
            err = PTR_ERR(dev->dev);
            goto fail_device;
        }

        dev->vaddr = vmalloc(HIXX_RINGBUFFER_SIZE);
        if (!dev->vaddr) {
            err = -ENOMEM;
            goto fail_vmalloc;
        }

        spin_lock_init(&dev->ring.lock);
        init_waitqueue_head(&dev->ring.wq);
        dev->ring.write_index = 0;
        dev->ring.read_index = 0;
        dev->ring.count = 0;
        spin_lock_init(&dev->metrics_lock);
        dev->inference_count = 0;

        pr_info("hixx: Device %d initialized (minor=%d)\n", i, i);
    }

    pr_info("hixx: Hixx-Server Kernel-Native Worker loaded. Major=%d\n", hixx_major);
    return 0;

fail_vmalloc:
    device_destroy(hixx_class, dev->devno);
fail_device:
    cdev_del(&dev->cdev);
fail_cdev:
    if (i > 0) {
        int j;
        for (j = 0; j < i; j++) {
            device_destroy(hixx_class, hixx_devices[j].devno);
            cdev_del(&hixx_devices[j].cdev);
            vfree(hixx_devices[j].vaddr);
        }
    }
    class_destroy(hixx_class);
    kfree(hixx_devices);
fail_class:
    unregister_chrdev_region(devno, HIXX_MAX_DEVICES);
fail_devices:
    return err;
}

static void __exit hixx_exit(void)
{
    int i;
    dev_t devno = MKDEV(hixx_major, 0);

    pr_info("hixx: Hixx-Server Kernel-Native Worker unloading...\n");

    for (i = 0; i < HIXX_MAX_DEVICES; i++) {
        struct hixx_device *dev = &hixx_devices[i];
        device_destroy(hixx_class, dev->devno);
        cdev_del(&dev->cdev);
        vfree(dev->vaddr);
    }
    class_destroy(hixx_class);
    kfree(hixx_devices);
    unregister_chrdev_region(devno, HIXX_MAX_DEVICES);
    pr_info("hixx: Hixx-Server unloaded\n");
}

module_init(hixx_init);
module_exit(hixx_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("Hixx-Server Team");
MODULE_DESCRIPTION("Hixx-Server Kernel-Native Inference Worker v2.0");
MODULE_VERSION("2.0.0");

