bits 32
global start

%assign NUM_PAGES 500
%assign JUMPS_PER_PAGE 20

section .text

start:
    mov ecx, 10
    jmp target_0_0

%assign p 0
%rep NUM_PAGES
    align 4096
    
    ; --- Generate 10 targets per page ---
    %assign t 0
    %rep JUMPS_PER_PAGE
target_%[p]_%[t]:

%if p < NUM_PAGES - 1
    ; If not the last page, jump to the same target index on the next page
    %assign next_p p+1
    jmp near target_%[next_p]_%[t]
    
%else
    ; If on the last page, wrap around to page 0 but move to the NEXT target index
    %if t < JUMPS_PER_PAGE - 1
        %assign next_t t+1
        jmp near target_0_%[next_t]
        
    %else
        ; If it's the last page AND the last target, we've completed all passes
        dec ecx
        jnz near target_0_0
        int 0xfe
    %endif
%endif

    %assign t t+1
    %endrep 
    ; ------------------------------------

    %assign p p+1
%endrep