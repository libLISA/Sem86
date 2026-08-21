bits 32
global start

NUM_JUMPS equ 200

section .text

start:
    mov ecx, 0x1000
    jmp target0

%assign i 0
%rep NUM_JUMPS
target%+i:

%if i < NUM_JUMPS - 1
    %assign next_i i+1
    jmp near target%+next_i
%else
    dec ecx
    jnz near target0
    int 0xfe
%endif

%assign i i+1
%endrep