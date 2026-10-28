// Emit a 512x512 source PNG for `cargo tauri icon`. No image library needed: a PNG is a
// signature plus a few length-prefixed, CRC-checked chunks around a zlib stream.
import zlib from 'node:zlib';
import fs from 'node:fs';

const SIZE = 512;

function crc32(buf) {
  let c;
  const table = [];
  for (let n = 0; n < 256; n++) {
    c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  let crc = 0xffffffff;
  for (const b of buf) crc = table[(crc ^ b) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

// An envelope glyph on a deep-blue rounded field.
const raw = Buffer.alloc(SIZE * (SIZE * 4 + 1));
let p = 0;
for (let y = 0; y < SIZE; y++) {
  raw[p++] = 0; // filter: none
  for (let x = 0; x < SIZE; x++) {
    const nx = x / SIZE;
    const ny = y / SIZE;

    // Rounded-square field.
    const r = 0.14;
    const dx = Math.max(r - nx, nx - (1 - r), 0);
    const dy = Math.max(r - ny, ny - (1 - r), 0);
    const outside = Math.hypot(dx, dy) > r;

    let [cr, cg, cb, ca] = [0, 0, 0, 0];
    if (!outside) {
      // Vertical gradient, Signet blue.
      cr = Math.round(24 + ny * 16);
      cg = Math.round(58 + ny * 40);
      cb = Math.round(120 + ny * 60);
      ca = 255;

      // Envelope body.
      const ex0 = 0.22, ex1 = 0.78, ey0 = 0.32, ey1 = 0.68;
      if (nx > ex0 && nx < ex1 && ny > ey0 && ny < ey1) {
        cr = 245; cg = 247; cb = 250;

        // Flap: two diagonals meeting in the middle.
        const t = (nx - ex0) / (ex1 - ex0);
        const flap = ey0 + Math.min(t, 1 - t) * (ey1 - ey0) * 0.95;
        const d = Math.abs(ny - flap);
        if (ny < flap + 0.012 && d < 0.05) {
          cr = 24; cg = 58; cb = 120;
        }
      }
    }
    raw[p++] = cr;
    raw[p++] = cg;
    raw[p++] = cb;
    raw[p++] = ca;
  }
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(SIZE, 0);
ihdr.writeUInt32BE(SIZE, 4);
ihdr[8] = 8;  // bit depth
ihdr[9] = 6;  // colour type: RGBA
ihdr[10] = 0; // deflate
ihdr[11] = 0; // adaptive filtering
ihdr[12] = 0; // no interlace

const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk('IHDR', ihdr),
  chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
  chunk('IEND', Buffer.alloc(0)),
]);

const out = process.argv[2] ?? 'app-icon.png';
fs.writeFileSync(out, png);
console.log(`wrote ${out} (${png.length} bytes, ${SIZE}x${SIZE})`);
