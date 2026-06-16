bits 32
global start

start:
    mov ecx, 0x1000

.loop:
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    pushf
    popf
    loop .loop
    int 0xfe