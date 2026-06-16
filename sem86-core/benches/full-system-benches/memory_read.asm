bits 32
global start

start:
    mov ecx, 0x10000
    mov eax, 0xf0000

.loop:
    mov eax, [eax + ecx * 8]
    mov eax, [eax + ecx * 8]
    mov eax, [eax + ecx * 8]
    mov eax, [eax + ecx * 8]
    mov eax, [eax + ecx * 8]
    mov eax, [eax + ecx * 8]
    mov eax, [eax + ecx * 8]
    mov eax, [eax + ecx * 8]
    loop .loop
    int 0xfe