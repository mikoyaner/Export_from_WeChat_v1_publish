// 语音 → 文字（离线）：silk-wasm 解码 silk→PCM@24000 → 拼 WAV → sherpa-onnx SenseVoice 识别。
// 仅在本机离线跑，音频不出机器。思路对齐 WeFlow_v2 的 transcribeWorker.ts（只借思路、独立实现）。
//
// 依赖**优先用随包的 vendor/**（silk-wasm / sherpa-onnx-node / sherpa-onnx-win-x64 + SenseVoice 模型），
// 这样整个目录拷到新电脑就能跑；vendor 不在时才退回 WEFLOW_ROOT 指向的 WeFlow_v2 安装。
// 覆盖顺序：显式环境变量 > ./vendor > WEFLOW_ROOT（默认 WeFlow_v2）。
//
// 用法：node voice2text.cjs <silk文件> [silk文件...]
// 输出：每行一个 JSON {file, ok, text, error}（诊断信息一律走 stderr，stdout 保持机器可读）
'use strict';
const fs = require('fs');
const path = require('path');
const { createRequire } = require('module');

const VENDOR = path.join(__dirname, 'vendor');

function resolvePath(value) {
  let s = String(value || '').trim();
  s = s.replace(/%([^%]+)%/g, (_, k) => process.env[k] || `%${k}%`);
  if (s === '~' || s.startsWith('~/') || s.startsWith('~\\')) {
    s = path.join(process.env.USERPROFILE || process.env.HOME || '', s.slice(1));
  }
  if (!s) return '';
  if (!path.isAbsolute(s)) s = path.join(__dirname, s);
  return path.normalize(s);
}

// WEFLOW_ROOT 只在 vendor 缺失时才需要（指向 WeFlow 项目目录）；不写死任何本机路径
const WF = resolvePath(process.env.WEFLOW_ROOT || '');

function pickDir(cands) {
  for (const c of cands) {
    if (c && fs.existsSync(c)) return c;
  }
  return null;
}

const NM = pickDir([
  resolvePath(process.env.SHERPA_NODE_MODULES || ''),
  path.join(VENDOR, 'node_modules'),
  WF ? path.join(WF, 'node_modules') : '',
]);
const MODEL_DIR = pickDir([
  resolvePath(process.env.SENSEVOICE_DIR || ''),
  path.join(VENDOR, 'models', 'sensevoice'),
  WF ? path.join(WF, 'resources', 'models', 'sensevoice') : '',
]);
if (!NM) {
  console.error('fatal: 找不到 silk-wasm / sherpa-onnx-node。请把 vendor/ 一起拷过来，'
                + '或设 WEFLOW_ROOT 指向带这些依赖的目录。');
  process.exit(2);
}
if (!MODEL_DIR) {
  console.error('fatal: 找不到 SenseVoice 模型目录（vendor/models/sensevoice 或 WEFLOW_ROOT 下）。');
  process.exit(2);
}
const MODEL = path.join(MODEL_DIR, 'model.int8.onnx');
const TOKENS = path.join(MODEL_DIR, 'tokens.txt');
const SAMPLE_RATE = 24000;

// 让 sherpa 的原生 DLL 可被找到（win-x64 包目录里带 c-api dll）
process.env.PATH = path.join(NM, 'sherpa-onnx-win-x64') + path.delimiter + process.env.PATH;

const req = createRequire(path.join(path.dirname(NM), 'package.json'));
console.error(`[voice] 依赖来自 ${NM.includes(VENDOR) ? '随包 vendor' : NM}` +
              `；模型来自 ${MODEL_DIR.includes(VENDOR) ? '随包 vendor' : MODEL_DIR}`);

function buildWav(pcm, sampleRate) {
  const header = Buffer.alloc(44);
  header.write('RIFF', 0);
  header.writeUInt32LE(36 + pcm.length, 4);
  header.write('WAVE', 8);
  header.write('fmt ', 12);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20);
  header.writeUInt16LE(1, 22); // mono
  header.writeUInt32LE(sampleRate, 24);
  header.writeUInt32LE(sampleRate * 2, 28);
  header.writeUInt16LE(2, 32);
  header.writeUInt16LE(16, 34);
  header.write('data', 36);
  header.writeUInt32LE(pcm.length, 40);
  return Buffer.concat([header, pcm]);
}

const TECH_TAGS = /<\|[^|]*\|>/g;
function cleanText(t) {
  return (t || '').replace(TECH_TAGS, '').replace(/\s+/g, ' ').trim();
}

async function main() {
  const files = process.argv.slice(2);
  if (files.length === 0) {
    console.error('用法: node voice2text.cjs <silk文件> [...]');
    process.exit(2);
  }
  const silk = req('silk-wasm');
  const sherpa = req('sherpa-onnx-node');
  const recognizer = new sherpa.OfflineRecognizer({
    modelConfig: {
      senseVoice: { model: MODEL, useInverseTextNormalization: 1 },
      tokens: TOKENS,
      numThreads: 2,
      debug: 0,
    },
  });

  for (const f of files) {
    const rec = { file: f, ok: false, text: '', error: '' };
    try {
      const silkData = fs.readFileSync(f);
      const decoded = await silk.decode(silkData, SAMPLE_RATE);
      const i16 = decoded.data; // Int16Array
      const pcm = Buffer.from(i16.buffer, i16.byteOffset, i16.byteLength);
      const wav = buildWav(pcm, SAMPLE_RATE);
      const pcmBody = wav.slice(44);
      const samples = new Float32Array(pcmBody.length / 2);
      for (let i = 0; i < samples.length; i++) samples[i] = pcmBody.readInt16LE(i * 2) / 32768.0;
      const stream = recognizer.createStream();
      stream.acceptWaveform({ sampleRate: SAMPLE_RATE, samples });
      recognizer.decode(stream);
      rec.text = cleanText(recognizer.getResult(stream).text);
      rec.ok = true;
    } catch (e) {
      rec.error = String((e && e.message) || e);
    }
    process.stdout.write(JSON.stringify(rec) + '\n');
  }
}

main().catch((e) => {
  console.error('fatal:', String(e));
  process.exit(1);
});
