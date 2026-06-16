bits 32
global start

start:
    mov ecx, 0x10000
    mov dx, 0x40 ; read timer 0

.loop:
    in al, dx
    in al, dx
    in al, dx
    in al, dx
    in al, dx
    in al, dx
    in al, dx
    in al, dx
    loop .loop
    int 0xfe