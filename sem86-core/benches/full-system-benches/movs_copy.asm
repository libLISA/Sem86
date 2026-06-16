bits 32
global start

start:
    mov ecx, 0x10000
    mov edi, 0x50000
    mov esi, 0x60000
    rep movsd
    int 0xfe