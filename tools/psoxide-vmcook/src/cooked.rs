//! Raw HMD8 (+ separate HLTX) viewmodel chunk: parse, edit, re-encode.
//! Layout follows hl-psx host/hl-bsp `cook_mdl` for `--mdl7-vm` output
//! (packed 5:5:5 normals, frame times, aligned SoA vertices, no hitboxes).

#[derive(Clone, Copy, Debug)]
pub struct Tri {
    pub idx: [u16; 3],
    pub tex: u16,
    pub uv: [u8; 6],
    pub norm: u16,
}

#[derive(Clone, Debug)]
pub struct Tex {
    pub w: u16,
    pub h: u16,
    pub clut: [u16; 16],
    pub pix4: Vec<u8>,
}

impl Tex {}

#[derive(Clone, Copy, Debug)]
pub struct Range {
    pub first: u16,
    pub count: u16,
    pub bone: u16,
    pub body: u8,
    pub flags: u8,
}

#[derive(Clone, Debug)]
pub struct Cooked {
    pub n_frames: usize,
    pub tex_hit: u32,
    pub l2w: u16,
    pub flags: u16,
    pub n_bones: usize,
    pub clips: Vec<[u16; 2]>,
    pub frame_times: Vec<u8>,
    pub ranges: Vec<Range>,
    pub verts: Vec<[i16; 3]>,
    pub poses: Vec<u8>,
    pub tail: Vec<u8>,
    pub tris: Vec<Tri>,
    pub texs: Vec<Tex>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sizes {
    pub header: usize,
    pub clips: usize,
    pub times_pad: usize,
    pub ranges: usize,
    pub verts: usize,
    pub poses: usize,
    pub tail: usize,
    pub tris: usize,
    pub tex_pix: usize,
    pub tex_clut: usize,
    pub tex_hdr: usize,
}

impl Sizes {
    pub fn geom(&self) -> usize {
        self.header
            + self.clips
            + self.times_pad
            + self.ranges
            + self.verts
            + self.poses
            + self.tail
            + self.tris
    }
    pub fn tex(&self) -> usize {
        self.tex_pix + self.tex_clut + self.tex_hdr
    }
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn i16le(b: &[u8], o: usize) -> i16 {
    i16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub const FLAG_FRAME_TIMES: u16 = 1 << 5;
pub const FLAG_PACKED_NORMALS: u16 = 1 << 2;

impl Cooked {
    pub fn parse(g: &[u8], t: &[u8]) -> Cooked {
        assert_eq!(&g[0..4], b"HMD8");
        let n_verts = u32le(g, 4) as usize;
        let n_tris = u32le(g, 8) as usize;
        let tex_hit = u32le(g, 12);
        let n_frames = u32le(g, 16) as usize;
        let n_clips = u32le(g, 20) as usize;
        let md_len = u32le(g, 24) as usize;
        let l2w = u16le(g, 28);
        let flags = u16le(g, 30);
        let n_bones = u16le(g, 32) as usize;
        let n_ranges = u16le(g, 34) as usize;
        assert!(
            flags & FLAG_PACKED_NORMALS != 0,
            "viewmodel cooks use packed normals"
        );
        let mut o = 36;
        let clips = (0..n_clips)
            .map(|c| [u16le(g, o + c * 4), u16le(g, o + c * 4 + 2)])
            .collect();
        o += n_clips * 4;
        let frame_times = if flags & FLAG_FRAME_TIMES != 0 {
            let v = g[o..o + n_frames].to_vec();
            o += n_frames;
            v
        } else {
            Vec::new()
        };
        while o & 3 != 0 {
            o += 1;
        }
        let md = &g[o..o + md_len];
        let ranges = (0..n_ranges)
            .map(|r| {
                let b = r * 8;
                Range {
                    first: u16le(md, b),
                    count: u16le(md, b + 2),
                    bone: u16le(md, b + 4),
                    body: md[b + 6],
                    flags: md[b + 7],
                }
            })
            .collect();
        let xy = n_ranges * 8;
        let z = xy + n_verts * 4;
        let verts = (0..n_verts)
            .map(|v| {
                [
                    i16le(md, xy + v * 4),
                    i16le(md, xy + v * 4 + 2),
                    i16le(md, z + v * 2),
                ]
            })
            .collect();
        let poses_off = z + n_verts * 2;
        let poses_len = n_frames * n_bones * 20;
        let poses = md[poses_off..poses_off + poses_len].to_vec();
        let tail = md[poses_off + poses_len..].to_vec();
        o += md_len;
        let tris = (0..n_tris)
            .map(|k| {
                let b = o + k * 16;
                let mut uv = [0u8; 6];
                uv.copy_from_slice(&g[b + 8..b + 14]);
                Tri {
                    idx: [u16le(g, b), u16le(g, b + 2), u16le(g, b + 4)],
                    tex: u16le(g, b + 6),
                    uv,
                    norm: u16le(g, b + 14),
                }
            })
            .collect();
        assert_eq!(o + n_tris * 16, g.len(), "trailing geometry bytes");
        assert_eq!(&t[0..4], b"HLTX");
        let nt = u32le(t, 4) as usize;
        let mut p = 8;
        let mut texs = Vec::new();
        for _ in 0..nt {
            let w = u16le(t, p);
            let h = u16le(t, p + 2);
            let mut clut = [0u16; 16];
            for (i, c) in clut.iter_mut().enumerate() {
                *c = u16le(t, p + 4 + i * 2);
            }
            let n = w as usize * h as usize / 2;
            texs.push(Tex {
                w,
                h,
                clut,
                pix4: t[p + 36..p + 36 + n].to_vec(),
            });
            p += 36 + n;
        }
        assert_eq!(p, t.len());
        Cooked {
            n_frames,
            tex_hit,
            l2w,
            flags,
            n_bones,
            clips,
            frame_times,
            ranges,
            verts,
            poses,
            tail,
            tris,
            texs,
        }
    }

    pub fn sizes(&self) -> Sizes {
        let clips = self.clips.len() * 4;
        let mut hdr_clips_times = 36 + clips + self.frame_times.len();
        let mut pad = 0;
        while (hdr_clips_times + pad) & 3 != 0 {
            pad += 1;
        }
        hdr_clips_times += pad;
        let _ = hdr_clips_times;
        Sizes {
            header: 36,
            clips,
            times_pad: self.frame_times.len() + pad,
            ranges: self.ranges.len() * 8,
            verts: self.verts.len() * 6,
            poses: self.poses.len(),
            tail: self.tail.len(),
            tris: self.tris.len() * 16,
            tex_pix: self.texs.iter().map(|t| t.pix4.len()).sum(),
            tex_clut: self.texs.len() * 32,
            tex_hdr: 8 + self.texs.len() * 4,
        }
    }

    pub fn geom_bytes(&self) -> Vec<u8> {
        let mut md = Vec::new();
        for r in &self.ranges {
            md.extend_from_slice(&r.first.to_le_bytes());
            md.extend_from_slice(&r.count.to_le_bytes());
            md.extend_from_slice(&r.bone.to_le_bytes());
            md.push(r.body);
            md.push(r.flags);
        }
        for v in &self.verts {
            md.extend_from_slice(&v[0].to_le_bytes());
            md.extend_from_slice(&v[1].to_le_bytes());
        }
        for v in &self.verts {
            md.extend_from_slice(&v[2].to_le_bytes());
        }
        md.extend_from_slice(&self.poses);
        md.extend_from_slice(&self.tail);
        let mut o = Vec::new();
        o.extend_from_slice(b"HMD8");
        o.extend_from_slice(&(self.verts.len() as u32).to_le_bytes());
        o.extend_from_slice(&(self.tris.len() as u32).to_le_bytes());
        let tex_hit = (self.texs.len() as u32 & 0xffff) | (self.tex_hit & 0xffff_0000);
        o.extend_from_slice(&tex_hit.to_le_bytes());
        o.extend_from_slice(&(self.n_frames as u32).to_le_bytes());
        o.extend_from_slice(&(self.clips.len() as u32).to_le_bytes());
        o.extend_from_slice(&(md.len() as u32).to_le_bytes());
        o.extend_from_slice(&self.l2w.to_le_bytes());
        o.extend_from_slice(&self.flags.to_le_bytes());
        o.extend_from_slice(&(self.n_bones as u16).to_le_bytes());
        o.extend_from_slice(&(self.ranges.len() as u16).to_le_bytes());
        for c in &self.clips {
            o.extend_from_slice(&c[0].to_le_bytes());
            o.extend_from_slice(&c[1].to_le_bytes());
        }
        o.extend_from_slice(&self.frame_times);
        while o.len() & 3 != 0 {
            o.push(0);
        }
        o.extend_from_slice(&md);
        for t in &self.tris {
            for i in t.idx {
                o.extend_from_slice(&i.to_le_bytes());
            }
            o.extend_from_slice(&t.tex.to_le_bytes());
            o.extend_from_slice(&t.uv);
            o.extend_from_slice(&t.norm.to_le_bytes());
        }
        o
    }

    pub fn tex_bytes(&self) -> Vec<u8> {
        let mut o = Vec::new();
        o.extend_from_slice(b"HLTX");
        o.extend_from_slice(&(self.texs.len() as u32).to_le_bytes());
        for t in &self.texs {
            o.extend_from_slice(&t.w.to_le_bytes());
            o.extend_from_slice(&t.h.to_le_bytes());
            for c in t.clut {
                o.extend_from_slice(&c.to_le_bytes());
            }
            o.extend_from_slice(&t.pix4);
        }
        o
    }

    /// Bone of every vertex (from the range table).
    pub fn vert_bone(&self) -> Vec<(u16, u8, u8)> {
        let mut out = vec![(0u16, 0u8, 0u8); self.verts.len()];
        for r in &self.ranges {
            for v in r.first as usize..(r.first + r.count) as usize {
                out[v] = (r.bone, r.body, r.flags);
            }
        }
        out
    }

    /// Keep only `keep[t]` triangles, then drop vertices no kept triangle
    /// references and rebuild the contiguous per-bone ranges. Triangle order
    /// is preserved, so the runtime's depth-bucket order among the survivors
    /// is unchanged.
    pub fn prune(&self, keep: &[bool]) -> Cooked {
        let mut c = self.clone();
        c.tris = self
            .tris
            .iter()
            .zip(keep)
            .filter(|(_, &k)| k)
            .map(|(t, _)| *t)
            .collect();
        let mut used = vec![false; self.verts.len()];
        for t in &c.tris {
            for i in t.idx {
                used[i as usize] = true;
            }
        }
        let vb = self.vert_bone();
        let mut remap = vec![u16::MAX; self.verts.len()];
        let mut verts = Vec::new();
        let mut ranges: Vec<Range> = Vec::new();
        for (v, &u) in used.iter().enumerate() {
            if !u {
                continue;
            }
            let n = verts.len() as u16;
            remap[v] = n;
            verts.push(self.verts[v]);
            let (bone, body, flags) = vb[v];
            match ranges.last_mut() {
                Some(r)
                    if r.bone == bone
                        && r.body == body
                        && r.flags == flags
                        && r.first + r.count == n =>
                {
                    r.count += 1
                }
                _ => ranges.push(Range {
                    first: n,
                    count: 1,
                    bone,
                    body,
                    flags,
                }),
            }
        }
        for t in &mut c.tris {
            for i in &mut t.idx {
                *i = remap[*i as usize];
            }
        }
        c.verts = verts;
        c.ranges = ranges;
        c
    }
}
