bits 32
global start

start:
    mov ecx, 0x1000

.loop:
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    pusha
    popa
    loop .loop
    int 0xfe