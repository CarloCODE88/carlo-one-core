obj-m += tri_ai_worker.o

CC = clang
LD = ld.lld

KERNEL_DIR := /lib/modules/$(shell uname -r)/build

all:
	$(MAKE) -C $(KERNEL_DIR) M=$(shell pwd) CC=$(CC) LD=$(LD) modules

clean:
	$(MAKE) -C $(KERNEL_DIR) M=$(shell pwd) CC=$(CC) LD=$(LD) clean
