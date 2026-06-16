bits 32
global start

start:
    mov ecx, 0x10000
    mov eax, 0xf0000

.loop:
    mov [eax + ecx * 8], eax
    mov [eax + ecx * 8], eax
    mov [eax + ecx * 8], eax
    mov [eax + ecx * 8], eax
    mov [eax + ecx * 8], eax
    mov [eax + ecx * 8], eax
    mov [eax + ecx * 8], eax
    mov [eax + ecx * 8], eax
    loop .loop
    int 0xfe