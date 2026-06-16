bits 32
global start
org 0x1000

start:
    mov ecx, 0x1000
.loop:
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]
    call dword [targets + 0]
    call dword [targets + 4]

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

%macro CALL_TARGET 1
%1:
    ret
%endmacro

CALL_TARGET target0
CALL_TARGET target1