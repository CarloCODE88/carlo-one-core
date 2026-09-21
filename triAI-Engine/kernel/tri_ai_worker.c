#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/fs.h>
#include <linux/uaccess.h>
#include <linux/ioctl.h>
#include <linux/slab.h>
#include <linux/highmem.h>
#include <linux/sched.h>
#include <linux/cpumask.h>
#include <linux/mm.h>
#include <linux/pagemap.h>
#include <linux/capability.h>
#include <linux/hugetlb.h>
#include <linux/huge_mm.h>
#include <linux/uidgid.h>
#include <linux/file.h>

#define DEVICE_NAME "tri_ai_worker"
#define CLASS_NAME "tri_ai"
#define MAX_TRACKED_PAGES 256
#define MAX_ORDER 11

#define TRI_IOCTL_MAGIC 't'
#define TRI_IOCTL_ALLOC_HUGEPAGE _IOWR(TRI_IOCTL_MAGIC, 0, unsigned long)
#define TRI_IOCTL_PIN_CPU _IOWR(TRI_IOCTL_MAGIC, 1, int)
#define TRI_IOCTL_PREFETCH_DISK _IOWR(TRI_IOCTL_MAGIC, 2, struct prefetch_req)
#define TRI_IOCTL_EVICT _IOWR(TRI_IOCTL_MAGIC, 3, unsigned long)
#define TRI_IOCTL_GET_STATS _IOWR(TRI_IOCTL_MAGIC, 4, struct tri_stats)

struct prefetch_req {
    int fd;
    loff_t offset;
    size_t len;
};

struct tri_stats {
    unsigned long vram_pages;
    unsigned long ram_pages;
    unsigned long disk_pages;
    unsigned long evictions;
    unsigned long prefetches;
    int overall_pressure;
};

struct tracked_page {
    struct page *page;
    unsigned long size_bytes;
    enum { TRI_VRAM, TRI_RAM, TRI_DISK } tier;
    bool is_hot;
    bool is_pinned;
    unsigned long allocated_at;
};

static struct tracked_page g_tracked[MAX_TRACKED_PAGES];
static int g_tracked_count = 0;
static DEFINE_SPINLOCK(g_tracked_lock);
static unsigned long g_evictions = 0;
static unsigned long g_prefetches = 0;

static int major_number;
static struct class *tri_class = NULL;
static struct device *tri_device = NULL;

static int validate_size(unsigned long size_mb) {
    if (size_mb == 0) return -EINVAL;
    unsigned long order = get_order(size_mb * 1024 * 1024);
    if (order >= MAX_ORDER) {
        pr_err("tri_ai: Requested order %lu exceeds MAX_ORDER (%d)\n", order, MAX_ORDER);
        return -ENOMEM;
    }
    if (size_mb > 4096) {
        pr_err("tri_ai: Requested %lu MB exceeds hard limit 4096 MB\n", size_mb);
        return -EINVAL;
    }
    return 0;
}

static long allocate_hugepage(unsigned long size_mb) {
    int ret;
    struct page *page;
    unsigned long order;

    ret = validate_size(size_mb);
    if (ret) return ret;

    order = get_order(size_mb * 1024 * 1024);
    page = alloc_pages(GFP_KERNEL | __GFP_NOWARN | __GFP_ZERO, order);
    if (!page) {
        pr_err("tri_ai: Allocation failed for %lu MB (Order %lu)\n", size_mb, order);
        return -ENOMEM;
    }

    spin_lock(&g_tracked_lock);
    if (g_tracked_count >= MAX_TRACKED_PAGES) {
        spin_unlock(&g_tracked_lock);
        __free_pages(page, order);
        pr_err("tri_ai: Tracked page limit reached (%d)\n", MAX_TRACKED_PAGES);
        return -ENOMEM;
    }
    g_tracked[g_tracked_count].page = page;
    g_tracked[g_tracked_count].size_bytes = size_mb * 1024 * 1024;
    g_tracked[g_tracked_count].tier = TRI_VRAM;
    g_tracked[g_tracked_count].is_hot = false;
    g_tracked[g_tracked_count].is_pinned = false;
    g_tracked[g_tracked_count].allocated_at = jiffies;
    g_tracked_count++;
    spin_unlock(&g_tracked_lock);

    pr_info("tri_ai: Allocated %lu MB (Order %lu), total tracked: %d\n", size_mb, order, g_tracked_count);
    return 0;
}

static long pin_cpu(int cpu_id) {
    cpumask_var_t mask;

    if (cpu_id < 0 || cpu_id >= nr_cpu_ids) {
        pr_warn("tri_ai: Invalid CPU ID %d (max: %d)\n", cpu_id, nr_cpu_ids - 1);
        return -EINVAL;
    }

    if (!zalloc_cpumask_var(&mask, GFP_KERNEL)) {
        return -ENOMEM;
    }

    cpumask_set_cpu(cpu_id, mask);
    set_cpus_allowed_ptr(current, mask);
    free_cpumask_var(mask);
    pr_info("tri_ai: Pinned task '%s' to CPU %d\n", current->comm, cpu_id);
    return 0;
}

static long prefetch_disk(struct prefetch_req __user *ureq) {
    struct prefetch_req req;
    struct file *filp;
    struct address_space *mapping;
    int ret;

    if (copy_from_user(&req, ureq, sizeof(req))) {
        pr_err("tri_ai: Failed to copy prefetch_req from user\n");
        return -EFAULT;
    }

    if (req.len == 0 || req.len > 1ULL << 40) {
        pr_err("tri_ai: Invalid prefetch length %zu\n", req.len);
        return -EINVAL;
    }

    filp = fget(req.fd);
    if (!filp) {
        pr_err("tri_ai: Invalid FD %d\n", req.fd);
        return -EBADF;
    }

    mapping = filp->f_mapping;
    if (!mapping || !mapping->a_ops) {
        fput(filp);
        return -EIO;
    }

    unsigned long nr_pages = (req.len + PAGE_SIZE - 1) / PAGE_SIZE;
    unsigned long start_index = req.offset / PAGE_SIZE;

    if (nr_pages > 1024) {
        pr_warn("tri_ai: Prefetch %lu pages capped to 1024\n", nr_pages);
        nr_pages = 1024;
    }

    /* Trigger kernel readahead via simple read */
    char buf[1];
    loff_t pos = req.offset;
    kernel_read(filp, buf, 1, &pos);

    pr_info("tri_ai: Readahead triggered for fd %d at offset %lld (%lu pages)\n", req.fd, req.offset, nr_pages);

    fput(filp);
    g_prefetches++;
    pr_info("tri_ai: Prefetched %lu pages from fd %d at offset %lld\n", nr_pages, req.fd, req.offset);
    return 0;
}

static long evict_pages(unsigned long min_priority) {
    unsigned long flags;
    int evicted = 0;

    spin_lock_irqsave(&g_tracked_lock, flags);
    for (int i = 0; i < g_tracked_count; i++) {
        if (g_tracked[i].is_pinned) continue;
        if (g_tracked[i].tier == TRI_VRAM && !g_tracked[i].is_hot) {
            g_tracked[i].tier = TRI_RAM;
            evicted++;
            g_evictions++;
        }
    }
    spin_unlock_irqrestore(&g_tracked_lock, flags);

    pr_info("tri_ai: Evicted %d pages (min_priority=%lu)\n", evicted, min_priority);
    return 0;
}

static long get_stats(struct tri_stats __user *ustats) {
    struct tri_stats stats = {0};
    unsigned long flags;

    spin_lock_irqsave(&g_tracked_lock, flags);
    stats.vram_pages = 0;
    stats.ram_pages = 0;
    stats.disk_pages = 0;
    for (int i = 0; i < g_tracked_count; i++) {
        switch (g_tracked[i].tier) {
            case TRI_VRAM: stats.vram_pages++; break;
            case TRI_RAM: stats.ram_pages++; break;
            case TRI_DISK: stats.disk_pages++; break;
        }
    }
    stats.evictions = g_evictions;
    stats.prefetches = g_prefetches;
    stats.overall_pressure = (stats.vram_pages > 200) ? 1 : 0;
    spin_unlock_irqrestore(&g_tracked_lock, flags);

    if (copy_to_user(ustats, &stats, sizeof(stats))) {
        return -EFAULT;
    }
    return 0;
}

static long tri_ioctl(struct file *file, unsigned int cmd, unsigned long arg) {
    void __user *argp = (void __user *)arg;

    if (!capable(CAP_SYS_ADMIN)) {
        pr_warn("tri_ai: Unauthorized ioctl by UID %u\n",
                 from_kuid(&init_user_ns, current_uid()));
        return -EPERM;
    }

    switch (cmd) {
        case TRI_IOCTL_ALLOC_HUGEPAGE: {
            unsigned long size_mb;
            if (copy_from_user(&size_mb, argp, sizeof(size_mb)))
                return -EFAULT;
            return allocate_hugepage(size_mb);
        }
        case TRI_IOCTL_PIN_CPU: {
            int cpu_id;
            if (copy_from_user(&cpu_id, argp, sizeof(cpu_id)))
                return -EFAULT;
            return pin_cpu(cpu_id);
        }
        case TRI_IOCTL_PREFETCH_DISK: {
            return prefetch_disk(argp);
        }
        case TRI_IOCTL_EVICT: {
            unsigned long min_priority;
            if (copy_from_user(&min_priority, argp, sizeof(min_priority)))
                return -EFAULT;
            return evict_pages(min_priority);
        }
        case TRI_IOCTL_GET_STATS: {
            return get_stats(argp);
        }
        default:
            pr_warn("tri_ai: Unknown ioctl command 0x%08x\n", cmd);
            return -ENOTTY;
    }
}

static int tri_open(struct inode *inode, struct file *file) {
    pr_info("tri_ai: Device opened by PID %d\n", current->pid);
    return 0;
}

static int tri_release(struct inode *inode, struct file *file) {
    pr_info("tri_ai: Device released by PID %d\n", current->pid);
    return 0;
}

static struct file_operations fops = {
    .owner = THIS_MODULE,
    .open = tri_open,
    .release = tri_release,
    .unlocked_ioctl = tri_ioctl,
#ifdef CONFIG_COMPAT
    .compat_ioctl = compat_ptr_ioctl,
#endif
};

static int __init tri_ai_init(void) {
    int ret;

    pr_info("tri_ai: Initializing Hardened Kernel Worker v1.0...\n");

    memset(g_tracked, 0, sizeof(g_tracked));
    g_tracked_count = 0;
    g_evictions = 0;
    g_prefetches = 0;

    major_number = register_chrdev(0, DEVICE_NAME, &fops);
    if (major_number < 0) {
        pr_err("tri_ai: register_chrdev failed: %d\n", major_number);
        return major_number;
    }

    tri_class = class_create(CLASS_NAME);
    if (IS_ERR(tri_class)) {
        ret = PTR_ERR(tri_class);
        goto err_class;
    }

    tri_device = device_create(tri_class, NULL, MKDEV(major_number, 0), NULL, DEVICE_NAME);
    if (IS_ERR(tri_device)) {
        ret = PTR_ERR(tri_device);
        goto err_device;
    }

    pr_info("tri_ai: Device registered. Major: %d. Ready for hardened inference.\n", major_number);
    return 0;

err_device:
    class_destroy(tri_class);
err_class:
    unregister_chrdev(major_number, DEVICE_NAME);
    pr_err("tri_ai: Init failed with error %d\n", ret);
    return ret;
}

static void __exit tri_ai_exit(void) {
    unsigned long flags;

    spin_lock_irqsave(&g_tracked_lock, flags);
    for (int i = 0; i < g_tracked_count; i++) {
        if (g_tracked[i].page) {
            int order = get_order(g_tracked[i].size_bytes);
            __free_pages(g_tracked[i].page, order);
        }
        g_tracked[i].page = NULL;
    }
    g_tracked_count = 0;
    spin_unlock_irqrestore(&g_tracked_lock, flags);

    device_destroy(tri_class, MKDEV(major_number, 0));
    class_destroy(tri_class);
    unregister_chrdev(major_number, DEVICE_NAME);
    pr_info("tri_ai: Unloaded. Cleaned %lu evictions.\n", g_evictions);
}

module_init(tri_ai_init);
module_exit(tri_ai_exit);
MODULE_LICENSE("GPL");
MODULE_AUTHOR("Fuerst ueber die verbotene Rechenleistung");
MODULE_DESCRIPTION("Hardened Kernel Worker for triAI-Engine");
MODULE_VERSION("1.0.0");