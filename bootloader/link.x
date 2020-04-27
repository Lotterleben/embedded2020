MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 1M
  /* RAM may not be used at all */
  RAM   : ORIGIN = 0x20000000, LENGTH = 0
}

ENTRY(Reset);

/* maximum stack usage = 8 bytes */
__stack_top__ = ORIGIN(RAM) + 8;

SECTIONS
{
  /* # Standard ELF sections */
  .vectors ORIGIN(FLASH) :
  {
    LONG(__stack_top__);
    LONG(Reset);
    LONG(DefaultHandler); /* NMI */
    LONG(DefaultHandler); /* HardFault */
    LONG(DefaultHandler); /* MemManage */
    LONG(DefaultHandler); /* BusFault */
    LONG(DefaultHandler); /* USageFault */
    LONG(0);              /* Reserved */
    LONG(0);              /* Reserved */
    LONG(0);              /* Reserved */
    LONG(0);              /* Reserved */
    LONG(DefaultHandler); /* SVCall */
    LONG(DefaultHandler); /* DebugMonitor */
    LONG(0);              /* Reserved */
    LONG(DefaultHandler); /* PendSV */
    LONG(DefaultHandler); /* SysTick */
  } > FLASH

  .text :
  {
    *(.text .text.*);
  } > FLASH

  .rodata :
  {
    *(.rodata .rodata.*);
  } > FLASH

  .bss :
  {
    KEEP(*(.bss .bss.*));
  } > RAM

  .data (NOLOAD) :
  {
    KEEP(*(.data .data.*));
  } > RAM

  /* ## Discarded sections */
  /DISCARD/ :
  {
    *(.ARM.exidx);
    *(.ARM.exidx.*);
    *(.ARM.extab.*);
  }
}

ASSERT(ADDR(.vectors) == 0, "vector table is misplaced");
ASSERT(SIZEOF(.vectors) == 64, "vector table has the wrong size");
ASSERT(SIZEOF(.bss) == 0, "static variables are forbidden");
ASSERT(SIZEOF(.data) == 0, "static variables are forbidden");
