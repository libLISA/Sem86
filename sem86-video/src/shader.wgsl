// Vertex shader

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) tex_coord: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
}

struct Mode {
    size: u32,
    config: u32,
}

@vertex
fn vs_main(
    model: VertexInput,
) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(model.position, 1.0);
    out.tex_coord = model.tex_coord;

    return out;
}

// Fragment shader
@group(0) @binding(0) var s: sampler;
@group(0) @binding(1) var font: texture_2d<f32>;
@group(0) @binding(2) var<uniform> mode: Mode;
@group(0) @binding(3) var<storage, read> framebuffer: array<u32>;
@group(0) @binding(4) var<storage, read> dac_palette: array<u32>;
@group(0) @binding(5) var<storage, read> vga_palette: array<u32>;
@group(0) @binding(6) var<uniform> window_size: vec2<u32>;

const MODE_CONFIG_BLINK: u32 = 0x01u;
const MODE_CONFIG_GRAPHICS: u32 = 0x02u;
const MODE_CONFIG_FORCE_43_ASPECT: u32 = 0x04;
const MODE_CONFIG_ADDRESSING: u32 = 0xFu << 3;

const ADDRESSING_ODD_EVEN: u32 = 0u << 3;
const ADDRESSING_CGA_ODD_EVEN: u32 = 1u << 3;
const ADDRESSING_SHIFT_REGISTER: u32 = 2u << 3;
const ADDRESSING_PLANAR4: u32 = 3u << 3;
const ADDRESSING_LINEAR8: u32 = 4u << 3;
const ADDRESSING_LINEAR15: u32 = 5u << 3;
const ADDRESSING_LINEAR16: u32 = 6u << 3;
const ADDRESSING_LINEAR24: u32 = 7u << 3;
const ADDRESSING_LINEAR32: u32 = 8u << 3;

// Converts a color from linear light gamma to sRGB gamma
fn fromLinear(linearRGB: vec4<f32>) -> vec4<f32> {
    let cutoff = linearRGB.rgb < vec3<f32>(0.0031308);
    let higher = vec3<f32>(1.055) * pow(linearRGB.rgb, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    let lower = linearRGB.rgb * vec3<f32>(12.92);
    let resultRGB = select(higher, lower, cutoff);
    return vec4<f32>(resultRGB, linearRGB.a);
}

// Converts a color from sRGB gamma to linear light gamma
fn toLinear(sRGB: vec4<f32>) -> vec4<f32> {
    let cutoff = sRGB.rgb < vec3<f32>(0.04045);
    let higher = pow((sRGB.rgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    let lower = sRGB.rgb / vec3<f32>(12.92);
    let resultRGB = select(higher, lower, cutoff);
    return vec4<f32>(resultRGB, sRGB.a);
}

fn postprocess_color(color: vec4<f32>) -> vec4<f32> {
    return toLinear(color);
}

fn u32_15bpp_to_color(rgb: u32) -> vec4<f32> {
    let r = f32(rgb & 0x1F) / 31.0;
    let g = f32((rgb >> 5) & 0x1F) / 31.0;
    let b = f32((rgb >> 10) & 0x1F) / 31.0;
    return vec4<f32>(r, g, b, 1.0);
}

fn u32_16bpp_to_color(rgb: u32) -> vec4<f32> {
    let r = f32(rgb & 0x1F) / 31.0;
    let g = f32((rgb >> 5) & 0x3F) / 63.0;
    let b = f32((rgb >> 11) & 0x1F) / 31.0;
    return vec4<f32>(r, g, b, 1.0);
}

fn u32_24bpp_to_color(rgb: u32) -> vec4<f32> {
    let b = f32(rgb & 0xFF) / 255.0;
    let g = f32((rgb >> 8) & 0xFF) / 255.0;
    let r = f32((rgb >> 16) & 0xFF) / 255.0;
    return vec4<f32>(r, g, b, 1.0);
}

fn u32_18bpp_to_color(rgb: u32) -> vec4<f32> {
    let r = f32(rgb & 0x3F) / 63.0;
    let g = f32((rgb >> 8) & 0x3F) / 63.0;
    let b = f32((rgb >> 16) & 0x3F) / 63.0;
    return vec4<f32>(r, g, b, 1.0);
}

fn palette_4bpp_to_color(idx: u32) -> vec4<f32> {
    return u32_18bpp_to_color(dac_palette[vga_palette[idx]]);
}

fn sample_color(grid_coord: vec2<f32>, grid_size: vec2<f32>) -> vec4<f32> {
    let blink_enabled = (mode.config & MODE_CONFIG_BLINK) != 0;
    let graphics_enabled = (mode.config & MODE_CONFIG_GRAPHICS) != 0;

    // TODO: This one isn't interpreted correctly for Windows 98. (sets to true, but needs false to render properly)
    let addressing = mode.config & MODE_CONFIG_ADDRESSING;
    let cga_odd_even = false;//(mode.config & MODE_CONFIG_CGA_ODD_EVEN) != 0;
    let shift_register_mode = addressing == ADDRESSING_SHIFT_REGISTER;
    let n256_color_mode = addressing == ADDRESSING_LINEAR8;

    let tilemap_size = vec2<f32>(8, 8 * 256);

    if !graphics_enabled {
        let cell_size = vec2<f32>(8, 8);
        let x = u32(grid_coord.x / cell_size.x);
        let y = u32(grid_coord.y / cell_size.y);
        
        // TODO: Use stride here instead
        let fb_index = y * u32(grid_size.x / cell_size.x) + x;
        let fb_word = framebuffer[u32(fb_index) * 2];
        let data = fb_word & 0xffff;

        let col_fg_idx = (data >> 8) & 0xf;
        let col_bg_idx = (data >> 12) & 0x7;
        let blink = ((data >> 15) & 1) != 0 && blink_enabled;

        let offset = data & 0xff;
        let col_bg = palette_4bpp_to_color(col_bg_idx);
        let col_fg = select(palette_4bpp_to_color(col_fg_idx), palette_4bpp_to_color(0u), blink);

        let tile_base = vec2<f32>(0, f32(offset * 8));
        let tile_coord = (tile_base + (grid_coord % cell_size)) / tilemap_size;

        // let col = vec3<f32>(0.5);
        let col = textureSample(font, s, tile_coord);
        return postprocess_color(mix(col_bg, col_fg, col.r));
    } else {
        let x = u32(grid_coord.x);
        let y = u32(grid_coord.y);

        var address: u32;
        switch addressing {
            case ADDRESSING_ODD_EVEN, ADDRESSING_PLANAR4: {
                // TODO: Use stride
                let pixel_index = u32(y) * u32(grid_size.x) + x;
                let byte_index = pixel_index / 8;

                address = byte_index;
            }
            case ADDRESSING_CGA_ODD_EVEN: {
                // TODO: Use stride
                let pixel_index = u32(y / 2) * u32(grid_size.x) + x;
                let byte_index = pixel_index / 8;
                
                address = byte_index + select(0u, 0x2000u, y % 2 == 1);
            }
            case ADDRESSING_SHIFT_REGISTER: {
                // TODO: Use stride
                let pixel_index = u32(y / 2) * u32(grid_size.x) + x;
                let byte_index = pixel_index / 4;
                
                address = byte_index + select(0u, 0x2000u, y % 2 == 1);
            }
            case ADDRESSING_LINEAR8, default: {
                // TODO: Use stride
                let pixel_index = u32(y) * u32(grid_size.x) + x;
                let byte_index = pixel_index / 4;

                address = byte_index;
            }
            case ADDRESSING_LINEAR15, ADDRESSING_LINEAR16: {
                // TODO: Use stride
                let pixel_index = u32(y) * u32(grid_size.x) + x;
                let byte_index = pixel_index / 2;

                address = byte_index;
            }
            case ADDRESSING_LINEAR24, ADDRESSING_LINEAR32: {
                // TODO: Use stride
                let pixel_index = u32(y) * u32(grid_size.x) + x;
                let byte_index = pixel_index;

                address = byte_index;
            }
        }

        switch addressing {
            case ADDRESSING_ODD_EVEN, ADDRESSING_PLANAR4, ADDRESSING_CGA_ODD_EVEN: {
                let fb_word = framebuffer[address];
                let bit0 = ((fb_word) >> (7 - (x % 8))) & 1;
                let bit1 = ((fb_word >> 8) >> (7 - (x % 8))) & 1;
                let bit2 = ((fb_word >> 16) >> (7 - (x % 8))) & 1;
                let bit3 = ((fb_word >> 24) >> (7 - (x % 8))) & 1;

                return postprocess_color(palette_4bpp_to_color(bit0 + bit1 * 2 + bit2 * 4 + bit3 * 8));
            }
            case ADDRESSING_SHIFT_REGISTER: {
                let fb_word = framebuffer[address & ~1u];
                let byte = fb_word >> select(0u, 8u, address % 2 == 1);
                let bit = (byte >> (6 - (x % 4) * 2)) & 3;

                return postprocess_color(palette_4bpp_to_color(bit * 4 + 3));
            }
            case ADDRESSING_LINEAR8, default: {
                let fb_word = framebuffer[address];
                let index = x % 4;
                let fb_byte = (fb_word >> (index * 8)) & 0xff;

                return postprocess_color(u32_18bpp_to_color(dac_palette[fb_byte]));
            }
            case ADDRESSING_LINEAR15: {
                let fb_word = framebuffer[address];
                let index = x % 2;
                let col = (fb_word >> (index * 16)) & 0xffff;

                return postprocess_color(u32_15bpp_to_color(col));
            }
            case ADDRESSING_LINEAR16: {
                let fb_word = framebuffer[address];
                let index = x % 2;
                let col = (fb_word >> (index * 16)) & 0xffff;

                return postprocess_color(u32_16bpp_to_color(col));
            }
            case ADDRESSING_LINEAR24: {
                let offset = (address * 3) / 4;
                let a = framebuffer[offset];
                let b = framebuffer[offset + 1];
                let index = address % 4;

                var col: u32;
                switch index {
                    case 0u, default: { col = a; }
                    case 1u: { col = (a >> 24) | (b << 8); }
                    case 2u: { col = (a >> 16) | (b << 16); }
                    case 3u: { col = a >> 8; }
                }
                
                return postprocess_color(u32_24bpp_to_color(col));
            }
            case ADDRESSING_LINEAR32: {
                let col = framebuffer[address];
                return postprocess_color(u32_24bpp_to_color(col));
            }
        }
    }
}


@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let grid_size = vec2<f32>(f32(mode.size & 0xffff), f32(mode.size >> 16));
    let force_43_aspect = (mode.config & MODE_CONFIG_FORCE_43_ASPECT) != 0;
    let expected_ratio = select(grid_size.x / grid_size.y, 4.0 / 3.0, force_43_aspect);

    let screen_ratio = f32(window_size.x) / f32(window_size.y);
    var in_bounds = true;

    var pos = in.tex_coord;
    var scale = 1.0;
    if (screen_ratio > expected_ratio) {
        // Bar on left/right
        scale = expected_ratio / screen_ratio;
        pos.x = pos.x / scale;
        pos.x += (1 - (1 / scale)) / 2;
    } else {
        // Bars on top/bottom
        scale = screen_ratio / expected_ratio;
        pos.y = pos.y / scale;
        pos.y += (1 - (1 / scale)) / 2;
    }

    // Letterboxing
    if (pos.x < 0 || pos.y < 0 || pos.x > 1 || pos.y > 1) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let box = grid_size / vec2<f32>(window_size) * .7;
    let box_size = min(box.x, box.y);
    let grid_coord = pos * grid_size;
    let fract = fract(grid_coord);
    let relative_fract = fract - box_size / 2;
    let full = sign(relative_fract);

    let c0 = sample_color(grid_coord, grid_size);
    let c1 = sample_color(grid_coord + full * vec2<f32>(box_size, 0), grid_size);
    let c2 = sample_color(grid_coord + full * vec2<f32>(0, box_size), grid_size);
    let c3 = sample_color(grid_coord + full * vec2<f32>(box_size, box_size), grid_size);

    let fx = abs(relative_fract.x);
    let fy = abs(relative_fract.y);

    return mix(mix(c0, c1, fx), mix(c2, c3, fx), fy);
}