bits 32
global start

start:
    mov ecx, 0x10000
.loop:
    imul eax, eax, 1103515245
    add eax, 12345
    test eax, 0x80000000
    jz .even
    inc ebx
    jmp .next

.even:
    dec ebx

.next:
    loop .loop
    int 0xfe