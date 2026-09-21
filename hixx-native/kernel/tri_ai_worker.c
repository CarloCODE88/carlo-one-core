#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/fs.h>
#include <linux/init.h>
#include <linux/ioctl.h>
#include <linux/kernel.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/uaccess.h>
#include <linux/vmalloc.h>

#define DEVICE_NAME "tri_ai_worker"
#define HIXX_IOC_MAGIC 0x48
#define RING_BUFFER_SIZE (4 * 1024 * 1024)

/* These commands have no input payload except GET_STATUS. */
#define HIXX_IOCTL_INIT_BUFFER _IO(HIXX_IOC_MAGIC, 0)
#define HIXX_IOCTL_SUBMIT_TASK _IO(HIXX_IOC_MAGIC, 1)
#define HIXX_IOCTL_GET_STATUS _IOR(HIXX_IOC_MAGIC, 2, int)

static dev_t dev_number;
static struct class *hixx_class;
static struct cdev hixx_cdev;
static void *ring_buffer;
static unsigned long head;
static unsigned long tail;
static DEFINE_MUTEX(hixx_lock);

static int hixx_open(struct inode *inode, struct file *file)
{
    return 0;
}

static long hixx_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
    int status;

    if (_IOC_TYPE(cmd) != HIXX_IOC_MAGIC)
        return -ENOTTY;

    mutex_lock(&hixx_lock);
    switch (cmd) {
    case HIXX_IOCTL_INIT_BUFFER:
        memset(ring_buffer, 0, RING_BUFFER_SIZE);
        head = 0;
        tail = 0;
        break;
    case HIXX_IOCTL_SUBMIT_TASK:
        /* This prototype only accounts for accepted work; it does not execute it. */
        head++;
        break;
    case HIXX_IOCTL_GET_STATUS:
        status = (int)(head - tail);
        mutex_unlock(&hixx_lock);
        if (copy_to_user((int __user *)arg, &status, sizeof(status)))
            return -EFAULT;
        return 0;
    default:
        mutex_unlock(&hixx_lock);
        return -ENOTTY;
    }
    mutex_unlock(&hixx_lock);
    return 0;
}

static int hixx_mmap(struct file *file, struct vm_area_struct *vma)
{
    unsigned long size = vma->vm_end - vma->vm_start;
    int err;

    if (vma->vm_pgoff != 0 || size != RING_BUFFER_SIZE) {
        pr_warn("hixx: invalid mmap offset=%lu size=%lu\n", vma->vm_pgoff, size);
        return -EINVAL;
    }

    err = remap_vmalloc_range(vma, ring_buffer, 0);
    if (err)
        pr_err("hixx: remap_vmalloc_range failed: %d\n", err);
    return err;
}

static const struct file_operations hixx_fops = {
    .owner = THIS_MODULE,
    .open = hixx_open,
    .unlocked_ioctl = hixx_ioctl,
    .mmap = hixx_mmap,
};

static int __init hixx_init(void)
{
    int err;
    struct device *device;

    ring_buffer = vmalloc_user(RING_BUFFER_SIZE);
    if (!ring_buffer)
        return -ENOMEM;

    err = alloc_chrdev_region(&dev_number, 0, 1, DEVICE_NAME);
    if (err)
        goto free_buffer;

    cdev_init(&hixx_cdev, &hixx_fops);
    err = cdev_add(&hixx_cdev, dev_number, 1);
    if (err)
        goto unregister_region;

    hixx_class = class_create("hixx");
    if (IS_ERR(hixx_class)) {
        err = PTR_ERR(hixx_class);
        goto delete_cdev;
    }

    device = device_create(hixx_class, NULL, dev_number, NULL, DEVICE_NAME);
    if (IS_ERR(device)) {
        err = PTR_ERR(device);
        goto destroy_class;
    }

    pr_info("hixx: loaded %s (major=%d, ring=%u bytes)\n", DEVICE_NAME,
            MAJOR(dev_number), RING_BUFFER_SIZE);
    return 0;

destroy_class:
    class_destroy(hixx_class);
delete_cdev:
    cdev_del(&hixx_cdev);
unregister_region:
    unregister_chrdev_region(dev_number, 1);
free_buffer:
    vfree(ring_buffer);
    ring_buffer = NULL;
    return err;
}

static void __exit hixx_exit(void)
{
    device_destroy(hixx_class, dev_number);
    class_destroy(hixx_class);
    cdev_del(&hixx_cdev);
    unregister_chrdev_region(dev_number, 1);
    vfree(ring_buffer);
    pr_info("hixx: unloaded\n");
}

module_init(hixx_init);
module_exit(hixx_exit);
MODULE_LICENSE("GPL");
MODULE_AUTHOR("HIXX");
MODULE_DESCRIPTION("HIXX shared-memory IPC prototype");
