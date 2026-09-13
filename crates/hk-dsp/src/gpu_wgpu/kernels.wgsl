// hk-dsp wgpu compute kernels (T-041): batched radix-2 FFT (with Bluestein for other lengths),
// STFT segment load, PFB polyphase fold, power rows and channel gather.
//
// Complex values are interleaved f32 pairs (re, im). A batch holds `count` items (segments or
// frames) of working length `p` (a power of two) in buffers `a` and `b`, item-major.
//
// Pipeline per batch:
//   load_stft | load_pfb   -> a   (bit-reversed order; Bluestein pre-chirp applied)
//   fft_stage x log2(p)    -> a   (in place, forward e^{-j2πkn/p})
//   [bluestein_mid         -> b ; fft_stage x log2(p) on b]
//   post_power | post_gather (source bound as binding 0: a, or b for Bluestein) -> out

struct Batch {
    count: u32,
    hop: u32,
    n: u32,
    p: u32,
    log_p: u32,
    width: u32,
    taps: u32,
    _pad: u32,
}

struct Stage {
    half: u32,
    log_half: u32,
    log_step: u32,
    _pad: u32,
}

struct Frame {
    offset: u32,
    first: u32,
    rot_re: f32,
    rot_im: f32,
}

@group(0) @binding(0) var<storage, read_write> a: array<f32>;
@group(0) @binding(1) var<storage, read_write> b: array<f32>;
@group(0) @binding(2) var<storage, read> input: array<f32>;
@group(0) @binding(3) var<storage, read_write> out: array<f32>;
@group(0) @binding(4) var<storage, read> batch: Batch;
@group(0) @binding(5) var<storage, read> frames: array<Frame>;
@group(0) @binding(6) var<storage, read> rev: array<u32>;
@group(0) @binding(7) var<storage, read> coef: array<f32>;
@group(0) @binding(8) var<storage, read> slot_coef: array<f32>;
@group(0) @binding(9) var<storage, read> kernel: array<f32>;
@group(0) @binding(10) var<storage, read> post_index: array<u32>;
@group(0) @binding(11) var<storage, read> post_coef: array<f32>;
@group(0) @binding(12) var<storage, read> selected: array<u32>;
@group(0) @binding(13) var<storage, read> twiddle: array<f32>;
@group(1) @binding(0) var<uniform> stage: Stage;

// 65535 workgroups x 256 invocations: the x extent of a two-dimensional dispatch.
const SPAN: u32 = 16776960u;

fn gindex(gid: vec3<u32>) -> u32 {
    return gid.x + gid.y * SPAN;
}

// STFT (and plain FFT): segment s = input[s*hop ..], times coef (window, or window x conj(chirp)).
@compute @workgroup_size(256)
fn load_stft(@builtin(global_invocation_id) gid: vec3<u32>) {
    let g = gindex(gid);
    let p = batch.p;
    if (g >= batch.count * p) {
        return;
    }
    let s = g >> batch.log_p;
    let i = g & (p - 1u);
    let j = rev[i];
    let o = 2u * g;
    if (j >= batch.n) {
        a[o] = 0.0;
        a[o + 1u] = 0.0;
        return;
    }
    let x = 2u * (s * batch.hop + j);
    let xr = input[x];
    let xi = input[x + 1u];
    let cr = coef[2u * j];
    let ci = coef[2u * j + 1u];
    a[o] = xr * cr - xi * ci;
    a[o + 1u] = xr * ci + xi * cr;
}

// PFB: slot k = Σ taps[t]·x[offset + t] over t ≡ k − first (mod M), times slot_coef[k].
@compute @workgroup_size(256)
fn load_pfb(@builtin(global_invocation_id) gid: vec3<u32>) {
    let g = gindex(gid);
    let p = batch.p;
    if (g >= batch.count * p) {
        return;
    }
    let f = g >> batch.log_p;
    let i = g & (p - 1u);
    let k = rev[i];
    let o = 2u * g;
    let m = batch.n;
    if (k >= m) {
        a[o] = 0.0;
        a[o + 1u] = 0.0;
        return;
    }
    let fr = frames[f];
    var sr: f32 = 0.0;
    var si: f32 = 0.0;
    for (var t: u32 = (k + m - fr.first) % m; t < batch.taps; t = t + m) {
        let x = 2u * (fr.offset + t);
        let c = 2u * t;
        let tr = coef[c];
        let ti = coef[c + 1u];
        let xr = input[x];
        let xi = input[x + 1u];
        sr = sr + tr * xr - ti * xi;
        si = si + tr * xi + ti * xr;
    }
    let sc = 2u * k;
    let cr = slot_coef[sc];
    let ci = slot_coef[sc + 1u];
    a[o] = sr * cr - si * ci;
    a[o + 1u] = sr * ci + si * cr;
}

// One butterfly stage over every item: pairs (i, i + half) in blocks of 2·half.
@compute @workgroup_size(256)
fn fft_stage(@builtin(global_invocation_id) gid: vec3<u32>) {
    let g = gindex(gid);
    let halfp = batch.p >> 1u;
    if (g >= batch.count * halfp) {
        return;
    }
    let s = g >> (batch.log_p - 1u);
    let t = g & (halfp - 1u);
    let h = stage.half;
    let blk = t >> stage.log_half;
    let k = t & (h - 1u);
    let i = (s << batch.log_p) + (blk << (stage.log_half + 1u)) + k;
    let l = i + h;
    let w = 2u * (k << stage.log_step);
    let wr = twiddle[w];
    let wi = twiddle[w + 1u];
    let lr = a[2u * l];
    let li = a[2u * l + 1u];
    let vr = lr * wr - li * wi;
    let vi = lr * wi + li * wr;
    let ur = a[2u * i];
    let ui = a[2u * i + 1u];
    a[2u * i] = ur + vr;
    a[2u * i + 1u] = ui + vi;
    a[2u * l] = ur - vr;
    a[2u * l + 1u] = ui - vi;
}

// Bluestein: b[i] = A[rev i] · G[rev i] (bit-reversed for the second transform).
@compute @workgroup_size(256)
fn bluestein_mid(@builtin(global_invocation_id) gid: vec3<u32>) {
    let g = gindex(gid);
    let p = batch.p;
    if (g >= batch.count * p) {
        return;
    }
    let s = g >> batch.log_p;
    let i = g & (p - 1u);
    let j = rev[i];
    let src = 2u * ((s << batch.log_p) + j);
    let ar = a[src];
    let ai = a[src + 1u];
    let kr = kernel[2u * j];
    let ki = kernel[2u * j + 1u];
    b[2u * g] = ar * kr - ai * ki;
    b[2u * g + 1u] = ar * ki + ai * kr;
}

// Power rows: out[s·N + i] = |post_coef[i] · D[post_index[i]]|² (tables already fftshifted).
@compute @workgroup_size(256)
fn post_power(@builtin(global_invocation_id) gid: vec3<u32>) {
    let g = gindex(gid);
    let n = batch.n;
    if (g >= batch.count * n) {
        return;
    }
    let s = g / n;
    let i = g - s * n;
    let src = 2u * ((s << batch.log_p) + post_index[i]);
    let dr = a[src];
    let di = a[src + 1u];
    let cr = post_coef[2u * i];
    let ci = post_coef[2u * i + 1u];
    out[g] = (dr * dr + di * di) * (cr * cr + ci * ci);
}

// Channel gather: out[f·A + j] = post_coef[k] · D[post_index[k]] · rot_f, k = active[j].
@compute @workgroup_size(256)
fn post_gather(@builtin(global_invocation_id) gid: vec3<u32>) {
    let g = gindex(gid);
    let wdt = batch.width;
    if (g >= batch.count * wdt) {
        return;
    }
    let f = g / wdt;
    let j = g - f * wdt;
    let k = selected[j];
    let src = 2u * ((f << batch.log_p) + post_index[k]);
    let dr = a[src];
    let di = a[src + 1u];
    let cr = post_coef[2u * k];
    let ci = post_coef[2u * k + 1u];
    let yr = dr * cr - di * ci;
    let yi = dr * ci + di * cr;
    let fr = frames[f];
    out[2u * g] = yr * fr.rot_re - yi * fr.rot_im;
    out[2u * g + 1u] = yr * fr.rot_im + yi * fr.rot_re;
}
