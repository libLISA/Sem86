bits 32
global start

NUM_JUMPS equ 1000

section .text

start:
    mov ecx, 10
    jmp target0

%assign i 0
%rep NUM_JUMPS
    align 4096
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