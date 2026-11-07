//! GoldSrc studio MDL reader: just what the viewmodel post-pass needs
//! (vertices, normals, triangles, textures). Offsets and decode rules follow
//! the games' host/hl-bsp `cook_mdl`, so the post-pass sees exactly the
//! source data the cooker sees.

pub struct Tex {
    pub flags: i32,
    pub w: usize,
    pub h: usize,
    pub pix: Vec<u8>,
    pub pal: Vec<[u8; 3]>,
}

pub struct Tri {
    pub v: [usize; 3],
    pub n: [usize; 3],
    pub st: [[f32; 2]; 3],
    pub tex: usize,
}

pub struct Mdl {
    pub verts: Vec<[f64; 3]>,
    pub vbone: Vec<usize>,
    pub norms: Vec<[f64; 3]>,
    pub tris: Vec<Tri>,
    pub texs: Vec<Tex>,
}

fn i32le(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn f32le(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn i16le(b: &[u8], o: usize) -> i16 {
    i16::from_le_bytes([b[o], b[o + 1]])
}

impl Mdl {
    pub fn load(path: &str, body_group: usize) -> Result<Mdl, String> {
        let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        if b.get(0..4) != Some(b"IDST") {
            return Err(format!("{path}: not IDST"));
        }
        let i = |o: usize| i32le(&b, o);
        let f = |o: usize| f32le(&b, o);
        // external textures
        let ext = i(180) == 0;
        let tbuf: Vec<u8> = if ext {
            let tp = format!("{}T.mdl", path.strip_suffix(".mdl").unwrap_or(path));
            std::fs::read(&tp).unwrap_or_default()
        } else {
            Vec::new()
        };
        let tb: &[u8] = if ext { &tbuf } else { &b };
        let mut texs = Vec::new();
        let (mut skinindex, mut numskinref, mut numtex) = (0usize, 0usize, 0usize);
        if tb.len() > 210 {
            let ti = |o: usize| i32le(tb, o);
            numtex = ti(180).max(0) as usize;
            let textureindex = ti(184) as usize;
            numskinref = ti(192).max(0) as usize;
            skinindex = ti(200) as usize;
            for t in 0..numtex {
                let to = textureindex + t * 80;
                let (w, h, idx) = (
                    ti(to + 68) as usize,
                    ti(to + 72) as usize,
                    ti(to + 76) as usize,
                );
                if idx + w * h + 768 > tb.len() {
                    texs.push(Tex {
                        flags: 0,
                        w: 1,
                        h: 1,
                        pix: vec![0],
                        pal: vec![[128, 128, 128]],
                    });
                    continue;
                }
                let pix = tb[idx..idx + w * h].to_vec();
                let pal = (0..256)
                    .map(|p| {
                        let o = idx + w * h + p * 3;
                        [tb[o], tb[o + 1], tb[o + 2]]
                    })
                    .collect();
                texs.push(Tex {
                    flags: ti(to + 64),
                    w,
                    h,
                    pix,
                    pal,
                });
            }
        }
        let (numbodyparts, bodypartindex) = (i(204) as usize, i(208) as usize);
        let mut verts = Vec::new();
        let mut vbone = Vec::new();
        let mut norms = Vec::new();
        let mut tris = Vec::new();
        for bp in 0..numbodyparts {
            let bpo = bodypartindex + bp * 76;
            let nummodels = i(bpo + 64).max(0) as usize;
            if nummodels == 0 {
                continue;
            }
            let base = i(bpo + 68).max(1) as usize;
            let selected = (body_group / base) % nummodels;
            let modelindex = i(bpo + 72) as usize + selected * 112;
            let (nummesh, meshindex) = (
                i(modelindex + 72).max(0) as usize,
                i(modelindex + 76) as usize,
            );
            let numverts = i(modelindex + 80).max(0) as usize;
            let (vinfoindex, vertindex) =
                (i(modelindex + 84) as usize, i(modelindex + 88) as usize);
            let vbase = verts.len();
            let nbase = norms.len();
            let numnorms = i(modelindex + 92).max(0) as usize;
            let (_ninfoindex, normindex) =
                (i(modelindex + 96) as usize, i(modelindex + 100) as usize);
            for nn in 0..numnorms {
                norms.push([
                    f(normindex + nn * 12) as f64,
                    f(normindex + nn * 12 + 4) as f64,
                    f(normindex + nn * 12 + 8) as f64,
                ]);
            }
            for v in 0..numverts {
                verts.push([
                    f(vertindex + v * 12) as f64,
                    f(vertindex + v * 12 + 4) as f64,
                    f(vertindex + v * 12 + 8) as f64,
                ]);
                vbone.push(b[vinfoindex + v] as usize);
            }
            for m in 0..nummesh {
                let me = meshindex + m * 20;
                let triindex = i(me + 4) as usize;
                let skinref = i(me + 8) as usize;
                let texid = if skinref < numskinref && !tb.is_empty() {
                    i16le(tb, skinindex + skinref * 2).max(0) as usize
                } else {
                    0
                };
                let mut o = triindex;
                loop {
                    let cmd = i16le(&b, o) as i32;
                    o += 2;
                    if cmd == 0 {
                        break;
                    }
                    let (n, fan) = (cmd.unsigned_abs() as usize, cmd < 0);
                    let mut s = Vec::with_capacity(n);
                    for _ in 0..n {
                        let lv = i16le(&b, o).max(0) as usize;
                        let ln = i16le(&b, o + 2).max(0) as usize;
                        let ss = i16le(&b, o + 4) as f32;
                        let tt = i16le(&b, o + 6) as f32;
                        o += 8;
                        s.push((vbase + lv, [ss, tt], nbase + ln));
                    }
                    for k in 0..n.saturating_sub(2) {
                        let (a, bb, c) = if fan {
                            (0, k + 1, k + 2)
                        } else if k % 2 == 0 {
                            (k, k + 1, k + 2)
                        } else {
                            (k + 1, k, k + 2)
                        };
                        tris.push(Tri {
                            v: [s[a].0, s[bb].0, s[c].0],
                            n: [s[a].2, s[bb].2, s[c].2],
                            st: [s[a].1, s[bb].1, s[c].1],
                            tex: texid.min(numtex.saturating_sub(1)),
                        });
                    }
                }
            }
        }
        Ok(Mdl {
            verts,
            vbone,
            norms,
            tris,
            texs,
        })
    }
}
