# Out-of-tree build of acer-wmi with the ANV15-51 quirk (temporary stopgap
# until kernel 7.3 ships it in-tree). See README.md.
obj-m += acer-wmi.o

KDIR ?= /lib/modules/$(shell uname -r)/build

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
