bits 32
global start
org 0x1000

start:
    ; -------------------------
    ; GDT setup (flat 32-bit)
    ; -------------------------
    lgdt [gdt_descriptor]

    mov ax, 0x10        ; data selector
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax
    mov esp, 0x90000    ; stack

    ; -------------------------
    ; Build IDT at runtime
    ; -------------------------
    mov eax, int0_handler
    mov edi, int0_handler
    shr edi, 16

    mov ecx, 1024
.set_entry:
    ; IDT entry = 1
    mov word [idt + ecx - 8 + 0], ax
    mov word [idt + ecx - 8 + 2], 0x08
    mov byte [idt + ecx - 8 + 4], 0
    mov byte [idt + ecx - 8 + 5], 0x8E
    mov word [idt + ecx - 8 + 6], di
    sub ecx, 8
    jnz .set_entry

    ; -------------------------
    ; Load IDT
    ; -------------------------
    lidt [idt_ptr]

    ; -------------------------
    ; Benchmark loop
    ; -------------------------
    mov ecx, 0x1000

bench_loop:
    int 1
    int 2
    int 3
    int 4

    int 1
    int 2
    int 3
    int 4

    int 1
    int 2
    int 3
    int 4

    int 1
    int 2
    int 3
    int 4
    loop bench_loop

    int 0xFE

; -------------------------
; Interrupt handler
; -------------------------
align 16
int0_handler:
    iret

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

align 128
idt_ptr:
    dw 256*8 - 1
    dd idt

align 128
idt:    resb 256*8