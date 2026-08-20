bits 32
global start

start:
    mov ecx, 0x1000

.loop:
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    push eax
    pop eax
    loop .loop
    int 0xfe