bits 32
global start

start:
    mov ecx, 0x10000
    mov dx, 0xFFFF ; unused port

.loop:
    out dx, al
    out dx, al
    out dx, al
    out dx, al
    out dx, al
    out dx, al
    out dx, al
    out dx, al
    loop .loop

    int 0xfe