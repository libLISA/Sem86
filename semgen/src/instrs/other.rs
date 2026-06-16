use liblisa::encoding::dataflows::{
    AccessKind, AddrTerm, AddrTermSize, AddressComputation, Inputs, MemoryAccess, MemorySizeRange, ParameterizedComputation,
};
use liblisa::encoding::{ParLoc, UnsizedParLoc};
use sem86_arch::exceptions::Exception;
use sem86_core::arch::intel386::{
    FLAG_IF, GpReg, HANDLER_CPUID, HANDLER_CS_UPDATED, HANDLER_HALT, HANDLER_INT, HANDLER_IRET, HANDLER_RDMSR, HANDLER_WRMSR,
    Intel386,
};
use sem86_core::il::{Cmd, Commands, Jump, Val};

use crate::builder::*;
use crate::context::{BuildFromContext, Context};
use crate::dsl::*;
use crate::instrs::{DWORD, EffectiveAddress, FLAG_VM, LOW_BYTE, invoke_gp};
use crate::{Config, encoding, encoding_group, ops};

pub fn gp_in_vm_ioplnot3_or(ops: Vec<Cmd<Intel386>>, ctx: &mut Context) -> Vec<Cmd<Intel386>> {
    ops! {
        #[context(ctx)]

        // temp0 is zero if IOPL=3
        let temp0 = xor((GpReg::Iopl, LOW_BYTE), 3);
        // This makes temp1 zero if VM = 0 or (VM = 1 and IOPL == 3)
        let vm = FLAG_VM;
        let temp1 = ite(vm, 0, temp0);


        if is_zero(temp1) {
            ..ops;
        } else {
            ..invoke_gp();
        }
    }
}

pub fn builder(_config: Config) -> impl Builder<Output = SemSpec<Intel386>> {
    [
        encoding! {
            Wide { true },
            Name { "LEA" },
            Prefixes, #0x8D,
            Mod = md, RegBits = reg, Rm { md },
            Filter::<Box<dyn Fn(&Context) -> bool>> { Box::new(move |_| md != ModVal::Mod11) },
            {
                BuildFromContext::new(move |ctx| {
                    let access = ctx.pop_access();
                    ops! {
                        #[context(ctx)]
                        reg := EffectiveAddress(&access);

                    }
                })
            }
        },
        encoding! {
            Name { "XLAT" },
            Prefixes, #0xD7,
            {
                BuildFromContext::new(move |ctx| {
                    let inputs = Inputs::unsorted(vec![
                        ctx.segment_override().unwrap_or(ParLoc { loc: UnsizedParLoc::Reg(GpReg::DsBase.into()), size: DWORD }),
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Bx.into()), size: DWORD },
                        ParLoc { loc: UnsizedParLoc::Reg(GpReg::Ax.into()), size: LOW_BYTE },
                    ]);
                    let mem = ctx.add_access(MemoryAccess {
                        kind: AccessKind::InputOutput,
                        size: MemorySizeRange::new(1, 1),
                        calculation: ParameterizedComputation::Calculation(AddressComputation::from_iter([
                            AddrTerm::single(AddrTermSize::U32, 0, 1),
                            AddrTerm::single(ctx.memory_reg_and_addr_size().0, 0, 1),
                            AddrTerm::single(AddrTermSize::U8, 0, 1),
                        ].into_iter(), 0).with_addr_size(ctx.memory_reg_and_addr_size().1)),
                        alignment: 1,
                        inputs,
                    });

                    vec![
                        Cmd::mov(Val::from((GpReg::Ax, LOW_BYTE)), Val::Loc(mem)),
                    ]
                })
            }
        },
        encoding_group! {
            [
                Name { "INT_n" },
                Prefixes, #0xCD,
                Imm { 8 } = ty,
            ] = ty,
            [
                Name { "INT_3" },
                Prefixes, #0xCC,
            ] = Val::const_val(3),
            map |n| BuildFromContext::new(move |ctx| SemSpec {
                commands: ops! {
                    #[context(ctx)]

                    // temp0 is zero if IOPL=3
                    let temp0 = xor((GpReg::Iopl, LOW_BYTE), 3);
                    // This makes temp1 zero if VM = 0 or (VM = 1 and IOPL == 3)
                    let vm = FLAG_VM;
                    let temp1 = ite(vm, 0, temp0);

                    if is_zero(temp1) {
                        ..Cmd::Handler {
                            id: HANDLER_INT,
                            args: vec![
                                n,
                                Val::Loc(ParLoc { loc: UnsizedParLoc::InstrLen, size: LOW_BYTE }),
                            ],
                        };
                    } else {
                        let temp0 = shl(n, 3);
                        let temp0 = or(temp0, 2);
                        ..Cmd::Exception {
                            exception: Exception::GeneralProtectionFault(0),
                            code: temp0,
                        };
                    }
                },
                // TODO: Natively recognise INT as a special operation
                jump: Jump::Far,
                ..Default::default()
            })
        },
        encoding_group! {
            [
                // Some CPUs don't decode the mod/rm byte of this instruction. Which ones? What should we emulate?
                Name { "UD0" },
                Prefixes, #0x0F, #0xFF, ModRm = (_reg, _rm),
            ] = (),
            [
                Name { "UD1" },
                Prefixes, #0x0F, #0xB9, ModRm = (_reg, _rm),
            ] = (),
            [
                Name { "UD2" },
                Prefixes, #0x0F, #0x0B,
            ] = (),
            map |_| BuildFromContext::new(move |_| vec![
                Cmd::Exception {
                    exception: Exception::InvalidOpcode,
                    code: Val::const_val(0),
                }
            ])
        },
        encoding! {
            Name { "INTO" },
            Prefixes, #0xCE,
            {
                // TODO: INTO
                BuildFromContext::new(move |_ctx| Vec::new())
            }
        },
        encoding! {
            Name { "BOUND" },
            Prefixes, #0x62,
            ModRm = (_reg, _rm),
            {
                // TODO: Bound
                BuildFromContext::new(move |_ctx| Vec::new())
            }
        },
        encoding! {
            Wide { true },
            Name { "IRET" },
            Prefixes, #0xCF,
            {
                BuildFromContext::new(move |ctx| SemSpec {
                    commands: gp_in_vm_ioplnot3_or(vec![
                        Cmd::Handler {
                            id: HANDLER_IRET,
                            args: vec![
                                Val::const_val(ctx.op_size() as u64)
                            ],
                        }
                    ], ctx),
                    // TODO: Natively recognise IRET as a special operation
                    jump: Jump::Far,
                    ..Default::default()
                })
            }
        },
        encoding! {
            Name { "HLT" },
            Prefixes, #0xF4,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::If {
                        val: Val::Loc(ParLoc {
                            loc: UnsizedParLoc::Reg(GpReg::Cpl.into()),
                            size: LOW_BYTE,
                        }),
                        if_zero: Commands::Ops(vec![
                            Cmd::Handler {
                                id: HANDLER_HALT,
                                args: Vec::new(),
                            },
                        ]),
                        if_nonzero: Commands::Ops(vec![
                            Cmd::Exception {
                                exception: Exception::GeneralProtectionFault(0),
                                code: Val::const_val(0),
                            }
                        ])
                    }
                ])
            }
        },
        encoding! {
            Name { "NOP" },
            Prefixes, #0x90,
            {
                BuildFromContext::new(move |_| vec![ ])
            }
        },
        encoding! {
            Name { "CPUID" },
            Prefixes, #0x0f, #0xa2,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::Handler {
                        id: HANDLER_CPUID,
                        args: vec! [
                            Val::from((GpReg::Ax, DWORD)),
                            Val::from((GpReg::Cx, DWORD)),
                        ]
                    }
                ])
            }
        },
        encoding! {
            Name { "RDMSR" },
            Prefixes, #0x0f, #0x32,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::Handler {
                        id: HANDLER_RDMSR,
                        args: vec! [
                            Val::from((GpReg::Cx, DWORD)),
                        ]
                    }
                ])
            }
        },
        encoding! {
            Name { "WRMSR" },
            Prefixes, #0x0f, #0x30,
            {
                BuildFromContext::new(move |_| vec![
                    Cmd::Handler {
                        id: HANDLER_WRMSR,
                        args: Vec::new()
                    }
                ])
            }
        },
        encoding! {
            Name { "WBINVD" },
            Prefixes, #0x0f, #0x09,
            {
                // TODO
                BuildFromContext::new(move |_| vec![])
            }
        },
        encoding! {
            Name { "RDTSC" },
            Prefixes, #0x0F, #0x31,
            ops! {
                // TODO: fault if (* CR4.TSD = 1 and (CPL = 1, 2, or 3) and CR0.PE = 1 *)
                #[context(ctx)]
                ..Cmd::Handler {
                    id: HANDLER_RDMSR,
                    args: vec![
                        0x10.into(),
                    ]
                };
            }
        },
        encoding! {
            Name { "SYSENTER" },
            Prefixes, #0x0F, #0x34,
            {
                // TODO: Do not add for 16-bit mode
                BuildFromContext::new(move |ctx| SemSpec {
                    commands: ops! {
                        #[context(ctx)]

                        // TODO: fault if not in protected mode or SysenterCs[15:2] = 0
                        FLAG_VM := 0;
                        FLAG_IF := 0;

                        GpReg::Ip := GpReg::SysEnterIp;
                        GpReg::Sp := GpReg::SysEnterSp;
                        GpReg::Cs := GpReg::SysEnterCs;
                        GpReg::CsBase := 0;
                        GpReg::CsLimit := 0xffff_ffffu64;
                        // TODO: Set GpReg::CsAr to executable and accessed

                        GpReg::Cpl := 0;

                        GpReg::Ss := add(GpReg::SysEnterCs, 8);
                        GpReg::SsBase := 0;
                        GpReg::SsLimit := 0xffff_ffffu64;
                        // TODO: Set GpReg::SsAr properly

                        ..Cmd::Handler {
                            id: HANDLER_CS_UPDATED,
                            args: Vec::new(),
                        };
                    },
                    jump: Jump::Far,
                    ..Default::default()
                })
            }
        },
        encoding! {
            Name { "SYSEXIT" },
            Prefixes, #0x0F, #0x35,
            {
                // TODO: Do not add for 16-bit mode
                BuildFromContext::new(move |ctx| SemSpec {
                    commands: ops! {
                        #[context(ctx)]

                        // TODO: fault if not in protected mode or SysenterCs[15:2] = 0

                        GpReg::Ip := GpReg::Dx;
                        GpReg::Sp := GpReg::Cx;
                        let cs = add(GpReg::SysEnterCs, 16);
                        let cs = or(cs, 3);
                        GpReg::Cs := cs;
                        GpReg::CsBase := 0;
                        GpReg::CsLimit := 0xffff_ffffu64;
                        // TODO: Set GpReg::CsAr to executable and accessed

                        GpReg::Cpl := 3;

                        GpReg::Ss := add(cs, 8);
                        GpReg::SsBase := 0;
                        GpReg::SsLimit := 0xffff_ffffu64;
                        // TODO: Set GpReg::SsAr properly

                        ..Cmd::Handler {
                            id: HANDLER_CS_UPDATED,
                            args: Vec::new(),
                        };
                    },
                    jump: Jump::Far,
                    ..Default::default()
                })
            }
        },
    ]
}
