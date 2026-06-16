bits 32
global start

start:
    mov ecx, 0x10000
    mov dx, 0xFFFF ; unused port

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