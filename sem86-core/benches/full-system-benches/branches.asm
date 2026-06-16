bits 32
global start

start:
    mov ecx, 0x10000
    xor eax, eax
    xor ebx, ebx
    xor edx, edx

.loop:
    inc eax
    test eax, 1
    jz .b1_taken
    nop
    jmp .b1_end

.b1_taken:
    inc ebx
.b1_end:

    cmp eax, 12345
    jl .b2_lt
    nop
    jmp .b2_end

.b2_lt:
    dec ebx
.b2_end:

    test ebx, 3
    jnz .b3_taken
    nop
    jmp .b3_end

.b3_taken:
    add edx, eax
.b3_end:

    dec edx
    jnz .b4_skip
    inc edx
.b4_skip:
    loop .loop

    int 0xfe