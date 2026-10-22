/*
 * FlashDB firmware harness -- linker script for STM32U545RE (NUCLEO-U545RE-Q,
 * Cortex-M33). Used only by the real-hardware test scripts; the QEMU
 * measurement firmware keeps using common/mps2_an385.ld.
 *
 * Memory map (STM32U545RE, TrustZone disabled / single flat address space):
 *   0x08000000..0x0807FFFF  512 KiB internal flash -> code + rodata + .data load
 *   0x20000000..0x2002FFFF  192 KiB SRAM1          -> .data/.bss/stack
 *   0x20030000..0x2003FFFF   64 KiB SRAM2          -> spare (not used here)
 *
 * As on the QEMU board, the simulated NOR flash backing store lives in its own
 * NOLOAD region at the top of SRAM1 so it is not counted as MCU static RAM.
 * The stack is the 8 KiB below it.
 */

ENTRY(Reset_Handler)

MEMORY
{
  FLASH (rx)  : ORIGIN = 0x08000000, LENGTH = 512K
  RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 192K
}

SECTIONS
{
  .text :
  {
    __text_start = .;
    KEEP(*(.vectors))
    *(.text*)
    *(.glue_7) *(.glue_7t) *(.vfp11_veneer) *(.v4_bx)
    . = ALIGN(4);
    __text_end = .;
  } > FLASH

  .rodata :
  {
    . = ALIGN(4);
    __rodata_start = .;
    *(.rodata*)
    . = ALIGN(4);
    __rodata_end = .;
  } > FLASH

  .data :
  {
    . = ALIGN(8);
    __data_start = .;
    *(.data*)
    *(.got) *(.got.plt)
    . = ALIGN(8);
    __data_end = .;
  } > RAM AT > FLASH
  _sidata = LOADADDR(.data);

  .bss (NOLOAD) :
  {
    . = ALIGN(8);
    __bss_start = .;
    *(.bss*)
    *(COMMON)
    . = ALIGN(8);
    __bss_end = .;
  } > RAM

  /* Backing store of the simulated NOR flash; kept even with --gc-sections. */
  .sim_flash (NOLOAD) :
  {
    . = ALIGN(4);
    __sim_flash_start = .;
    KEEP(*(.sim_flash))
    . = ALIGN(4);
    __sim_flash_end = .;
  } > RAM

  /* Board result buffer read back by the probe (see board-u545 feature). */
  .results (NOLOAD) :
  {
    . = ALIGN(8);
    KEEP(*(.results))
    . = ALIGN(8);
  } > RAM

  _stack_size   = 0x2000;                    /* 8 KiB */
  _estack       = ORIGIN(RAM) + 160K;        /* top of the used SRAM1 area */
  _stack_bottom = _estack - _stack_size;

  _sdata = __data_start;
  _edata = __data_end;
  _sbss  = __bss_start;
  _ebss  = __bss_end;

  /DISCARD/ :
  {
    *(.ARM.exidx*)
    *(.ARM.extab*)
    *(.note*)
    *(.comment)
  }
}
