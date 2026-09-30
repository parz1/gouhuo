// SPDX-License-Identifier: MPL-2.0
const alphabet = "0123456789abcdefghjkmnpqrstvwxyz";

export function encodeBase32(bytes) {
  let buffer = 0, bits = 0, result = "";
  for (const byte of bytes) {
    buffer = ((buffer << 8) | byte) & 0xffff;
    bits += 8;
    while (bits >= 5) { bits -= 5; result += alphabet[(buffer >>> bits) & 31]; }
  }
  if (bits) result += alphabet[(buffer << (5 - bits)) & 31];
  return result;
}

// 与 protocol::Invite v1 一致。输入只含公开的服务器身份，不含服务端加入码。
export async function buildInvite(payloadHex, code = "") {
  if (!/^(?:[0-9a-f]{2})+$/.test(payloadHex)) throw new Error("服务器连接信息有误，请联系部署者。");
  const base = Uint8Array.from(payloadHex.match(/../g), byte => parseInt(byte, 16));
  if (base[0] !== 1 || base[1] !== 0 || base.length !== 21 + base[20] || !base[20]) {
    throw new Error("服务器邀请格式不受支持，请联系部署者。");
  }
  const credential = new TextEncoder().encode(code);
  if (credential.length > 255) throw new Error("加入码太长，请检查复制的内容。");
  const body = new Uint8Array(base.length + (code ? credential.length + 1 : 0));
  body.set(base);
  if (code) { body[1] = 1; body[base.length] = credential.length; body.set(credential, base.length + 1); }
  if (!globalThis.crypto?.subtle) throw new Error("请通过 HTTPS 打开此页面，再尝试加入。");
  const checksum = new Uint8Array(await crypto.subtle.digest("SHA-256", body));
  const bytes = new Uint8Array(body.length + 2);
  bytes.set(body); bytes.set(checksum.subarray(0, 2), body.length);
  return "gouhuo://j/" + encodeBase32(bytes);
}

if (typeof document !== "undefined") {
  const join = document.getElementById("join"), copy = document.getElementById("copy");
  const code = document.getElementById("join-code"), field = document.getElementById("code-field");
  const status = document.getElementById("status"), manual = document.getElementById("manual");
  const manualLink = document.getElementById("manual-link");
  const privateServer = document.body.dataset.private === "true";
  let revision = 0;
  // fragment 不会发送给服务端；读完从地址栏移除，不保存到 localStorage。
  function readFragment() {
    const fragment = new URLSearchParams(location.hash.slice(1));
    if (fragment.has("code")) {
      code.value = fragment.get("code");
      field.hidden = false;
      history.replaceState(null, "", location.pathname + location.search);
    }
  }
  readFragment();
  function message(text, error = false) { status.textContent = text; status.classList.toggle("error", error); }
  async function prepare() {
    const current = ++revision;
    join.removeAttribute("href"); join.setAttribute("aria-disabled", "true"); join.tabIndex = -1; copy.disabled = true;
    manual.hidden = true; manualLink.value = "";
    const credential = code.value.trim();
    if (privateServer && !credential) { message("输入加入码后，就可以打开篝火。"); return; }
    try {
      const link = await buildInvite(document.body.dataset.payload, credential);
      if (current !== revision) return;
      join.href = link; join.setAttribute("aria-disabled", "false"); join.tabIndex = 0; copy.disabled = false;
      message("浏览器可能询问是否打开篝火，请选择允许。");
    } catch (error) { if (current === revision) message(error.message, true); }
  }
  code.addEventListener("input", prepare);
  window.addEventListener("hashchange", () => { readFragment(); prepare(); });
  code.addEventListener("keydown", event => { if (event.key === "Enter" && join.hasAttribute("href")) join.click(); });
  join.addEventListener("click", event => {
    if (!join.hasAttribute("href")) { event.preventDefault(); return; }
    message("已请求打开篝火。如果没有打开，可下载客户端或复制邀请链接。");
  });
  copy.addEventListener("click", async () => {
    const link = join.getAttribute("href");
    if (!link) return;
    try { await navigator.clipboard.writeText(link); message("邀请已复制，粘贴到篝火即可加入。"); }
    catch { manual.hidden = false; manualLink.value = link; manualLink.focus(); manualLink.select(); message("浏览器未允许复制，请手动复制下方链接。"); }
  });
  prepare();
}
