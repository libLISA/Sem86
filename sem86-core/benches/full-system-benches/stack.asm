bits 32
global start

start:
    mov ecx, 0x10000

.loop:
    push ecx
    push ecx
    push ecx
    push ecx
    pop ecx
    pop ecx
    pop ecx
    pop ecx
    push ecx
    push ecx
    push ecx
    push ecx
    pop ecx
    pop ecx
    pop ecx
    pop ecx
    loop .loop
    int 0xfe