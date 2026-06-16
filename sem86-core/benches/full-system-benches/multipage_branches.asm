bits 32
global start

align 4096
start:
    mov ecx, 0x10000
    xor eax, eax
    xor ebx, ebx
page0:
loop0:
    inc eax
    test eax, 1
    jz page1_branch0
    inc ebx
    jmp page1_common

times (4096 - ($ - page0)) nop

align 4096
page1:

page1_branch0:
    dec ebx

page1_common:
    test ebx, 2
    jnz page0_branch1

    inc eax
    jmp page0_common

page0_branch1:
    add eax, ebx

page0_common:
    dec ecx
    jnz loop0
    int 0xfe