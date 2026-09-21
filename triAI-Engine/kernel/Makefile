obj-m += tri_ai_worker.o

KDIR := /usr/lib/modules/$(shell uname -r)/build
PWD := $(shell pwd)
CC = clang
LD = ld.lld

all:
	@echo ">>> ELIMINATING COMPATIBILITY ENEMY <<<"
	$(MAKE) -C $(KDIR) M=$(PWD) CC=$(CC) LD=$(LD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

load:
	sudo insmod tri_ai_worker.ko
	dmesg | tail -n 5

unload:
	sudo rmmod tri_ai_worker
	dmesg | tail -n 5
