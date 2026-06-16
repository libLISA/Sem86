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
    dd target0
    dd target1
    dd target2
    dd target3
    dd target4
    dd target5
    dd target6
    dd target7
    dd target8
    dd target9
    dd target10
    dd target11
    dd target12
    dd target13
    dd target14
    dd target15
    dd target0
    dd target1
    dd target2
    dd target3
    dd target4
    dd target5
    dd target6
    dd target7
    dd target8
    dd target9
    dd target10
    dd target11
    dd target12
    dd target13
    dd target14
    dd target15

%macro CALL_TARGET 1
%1:
    nop
    ret
%endmacro

CALL_TARGET target0
CALL_TARGET target1
CALL_TARGET target2
CALL_TARGET target3
CALL_TARGET target4
CALL_TARGET target5
CALL_TARGET target6
CALL_TARGET target7
CALL_TARGET target8
CALL_TARGET target9
CALL_TARGET target10
CALL_TARGET target11
CALL_TARGET target12
CALL_TARGET target13
CALL_TARGET target14
CALL_TARGET target15