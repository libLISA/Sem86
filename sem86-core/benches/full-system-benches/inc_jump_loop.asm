bits 32
global start

start:
    mov ecx, 0x10000
    xor eax, eax
.loop:
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    inc eax
    jc .never
    loop .loop
    int 0xfe
.never:
    jmp .never