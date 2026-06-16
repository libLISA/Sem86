bits 32
global start
org 0x1000

start:
    lgdt [gdt_descriptor]

    mov ecx, 0x10000
    mov ax, 0x10        ; data selector
bench_loop:
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    loop bench_loop
    int 0xFE

align 128
gdt_start:
    dq 0x0000000000000000        ; Null descriptor
    dq 0x00CF9A000000FFFF        ; Code segment, base=0, limit=4GB, DPL=0
    dq 0x00CF92000000FFFF        ; Data segment, base=0, limit=4GB, DPL=0
gdt_end:

align 128
gdt_descriptor:
    dw gdt_end - gdt_start - 1
    dd gdt_start