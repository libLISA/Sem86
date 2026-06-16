bits 32
global start

start:
    mov ecx, 0x10000

.loop:
    mov eax, [0xFEE00000]
    mov eax, [0xFEE00000]
    mov eax, [0xFEE00000]
    mov eax, [0xFEE00000]

    mov eax, [0xFEE00000]
    mov eax, [0xFEE00000]
    mov eax, [0xFEE00000]
    mov eax, [0xFEE00000]
    
    loop .loop
    int 0xfe