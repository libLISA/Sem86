bits 32
global start

PAGE_SIZE equ 4096

section .text

start:
    mov ecx, 0x1000
    jmp block11

align PAGE_SIZE
block0:
    jne near block1
block2:
    jne near block3
block4:
    jne near block5
block6:
    jne near block7
block8:
    jne near block9
block10:
    jne near block11
block1:
    jne near block2
block3:
    jne near block4
block5:
    jne near block6
block7:
    jne near block8
block9:
    jne near block10
block11:
    dec ecx
    jne block0
    int 0xfe