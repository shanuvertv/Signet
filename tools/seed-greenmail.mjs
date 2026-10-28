// Minimal SMTP client: seeds GreenMail with the same fixtures the POP3 suite uses.
import net from 'node:net';

const HOST = '127.0.0.1';
const PORT = 3025;

function send(raw, from, to) {
  return new Promise((resolve, reject) => {
    const sock = net.createConnection(PORT, HOST);
    const steps = [
      'EHLO spike.invalid',
      `MAIL FROM:<${from}>`,
      `RCPT TO:<${to}>`,
      'DATA',
    ];
    let i = -1; // -1 = waiting for greeting
    let inData = false;
    let buf = '';

    sock.on('data', (chunk) => {
      buf += chunk.toString('utf8');
      let idx;
      while ((idx = buf.indexOf('\r\n')) !== -1) {
        const line = buf.slice(0, idx);
        buf = buf.slice(idx + 2);
        // Skip multi-line continuations (e.g. "250-STARTTLS").
        if (/^\d{3}-/.test(line)) continue;
        const code = parseInt(line.slice(0, 3), 10);
        if (code >= 400) return reject(new Error(`server said: ${line}`));

        if (inData) {
          sock.write('QUIT\r\n');
          sock.end();
          return resolve();
        }
        i += 1;
        if (i < steps.length) {
          sock.write(steps[i] + '\r\n');
        } else {
          // Byte-stuff: any body line starting with '.' gets one prepended.
          const stuffed = raw
            .split('\r\n')
            .map((l) => (l.startsWith('.') ? '.' + l : l))
            .join('\r\n');
          sock.write(stuffed + '\r\n.\r\n');
          inData = true;
        }
      }
    });
    sock.on('error', reject);
  });
}

const now = new Date().toUTCString();

const messages = [
  {
    name: 'plain',
    raw: [
      'From: Alice <alice@example.com>',
      'To: signet@example.com',
      'Subject: Phase 0 smoke test',
      `Date: ${now}`,
      'Message-ID: <spike-1@example.com>',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Nothing unusual about this one.',
    ].join('\r\n'),
  },
  {
    name: 'dot-stuffed',
    raw: [
      'From: Bob <bob@example.com>',
      'To: signet@example.com',
      'Subject: Lines that start with a dot',
      `Date: ${now}`,
      'Message-ID: <spike-2@example.com>',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'The next line is a single dot, which must survive the round trip:',
      '.',
      '..and this one begins with two.',
      '. leading dot then text',
      'done',
    ].join('\r\n'),
  },
  {
    name: 'utf8-and-encoded-words',
    raw: [
      'From: =?utf-8?B?SMOpbMOobmU=?= <helene@example.com>',
      'To: signet@example.com',
      'Subject: =?utf-8?Q?Lumi=C3=A8re_=E2=80=94_r=C3=A9sum=C3=A9?=',
      `Date: ${now}`,
      'Message-ID: <spike-3@example.com>',
      'MIME-Version: 1.0',
      'Content-Type: text/plain; charset=utf-8',
      'Content-Transfer-Encoding: 8bit',
      '',
      'Accents: éàüñ 日本語 — em dash.',
    ].join('\r\n'),
  },
  {
    name: 'large-multipart',
    raw: [
      'From: Reports <reports@example.com>',
      'To: signet@example.com',
      'Subject: Attachment, roughly 512 KiB',
      `Date: ${now}`,
      'Message-ID: <spike-4@example.com>',
      'MIME-Version: 1.0',
      'Content-Type: multipart/mixed; boundary="BOUND"',
      '',
      '--BOUND',
      'Content-Type: text/plain; charset=utf-8',
      '',
      'Body plus a big attachment, to prove streaming.',
      '--BOUND',
      'Content-Type: application/octet-stream; name="blob.bin"',
      'Content-Transfer-Encoding: base64',
      'Content-Disposition: attachment; filename="blob.bin"',
      '',
      ...Array.from({ length: 8192 }, () => 'QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2'),
      '--BOUND--',
    ].join('\r\n'),
  },
];

for (const m of messages) {
  await send(m.raw, 'sender@example.com', 'signet@example.com');
  console.log(`sent: ${m.name} (${m.raw.length} bytes)`);
}
