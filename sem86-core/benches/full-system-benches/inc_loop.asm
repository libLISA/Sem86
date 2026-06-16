bits 32
global start

start:
    mov ecx, 0x10000

.loop:
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    inc eax
    loop .loop
    int 0xfe