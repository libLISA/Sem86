bits 32
global start
org 0x1000

start:
    mov ecx, 0x1000
    mov esi, 0x12345678

.loop:
    imul esi, 37
    mov edx, esi
    shr edx, 28
    call dword [targets + edx*4]
    call dword [targets + edx*4 + 4]
    call dword [targets + edx*4 + 32]
    call dword [targets + edx*4 + 16]
    call dword [targets + edx*4 + 8]
    call dword [targets + edx*4 + 48]
    call dword [targets + edx*4 + 60]
    call dword [targets + edx*4 + 44]
    call dword [targets + edx*4 + 52]
    call dword [targets + edx*4 + 12]
    call dword [targets + edx*4]
    call dword [targets + edx*4 + 4]
    call dword [targets + edx*4 + 32]
    call dword [targets + edx*4 + 16]
    call dword [targets + edx*4 + 8]
    call dword [targets + edx*4 + 48]
    call dword [targets + edx*4 + 60]
    call dword [targets + edx*4 + 44]
    call dword [targets + edx*4 + 52]
    call dword [targets + edx*4 + 12]
    call dword [targets + edx*4]
    call dword [targets + edx*4 + 4]
    call dword [targets + edx*4 + 32]
    call dword [targets + edx*4 + 16]
    call dword [targets + edx*4 + 8]
    call dword [targets + edx*4 + 48]
    call dword [targets + edx*4 + 60]
    call dword [targets + edx*4 + 44]
    call dword [targets + edx*4 + 52]
    call dword [targets + edx*4 + 12]

.loop_tail:
    dec ecx
    jnz .loop
    int 0xfe

; -------------------------------------------------
; Indirect call table
; -------------------------------------------------
align 16
targets:
    dd page0
    dd page1
    dd page2
    dd page3
    dd page4
    dd page5
    dd page6
    dd page7
    dd page8
    dd page9
    dd page10
    dd page11
    dd page12
    dd page13
    dd page14
    dd page15
    dd page0
    dd page1
    dd page2
    dd page3
    dd page4
    dd page5
    dd page6
    dd page7
    dd page8
    dd page9
    dd page10
    dd page11
    dd page12
    dd page13
    dd page14
    dd page15

%macro PAGE 1
align 4096
%1:
    ret
times (4096 - ($ - %1)) nop
%endmacro

PAGE page0
PAGE page1
PAGE page2
PAGE page3
PAGE page4
PAGE page5
PAGE page6
PAGE page7
PAGE page8
PAGE page9
PAGE page10
PAGE page11
PAGE page12
PAGE page13
PAGE page14
PAGE page15