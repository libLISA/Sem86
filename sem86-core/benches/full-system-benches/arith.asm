bits 32
global start

start:
    mov ecx, 0x10000

.loop:
    add eax, ebx
    sub ebx, eax
    shl eax, 12
    shr ebx, 2
    inc eax
    imul eax, ebx
    dec ebx
    adc eax, ebx
    sbb ebx, ecx
    loop .loop
    int 0xfe