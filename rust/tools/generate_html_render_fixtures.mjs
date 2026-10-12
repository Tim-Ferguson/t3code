// Writes crates/server/tests/fixtures/html-render-shared.jsonl, public-proxy-addresses.jsonl and html-render-images.jsonl:
// what the original does with the shared HTML render helpers (packages/shared/src/htmlRender.ts), how the preview proxy's
// address guard (apps/server/src/htmlRender/publicProxy.ts) classifies addresses, and how the page's local images are
// found and recognised (apps/server/src/htmlRender/HtmlRender.ts), as cases html_render_shared_tests.rs,
// public_proxy_tests.rs and html_render_tests.rs check the Rust port against. Development-only; the Rust tests never run
// TypeScript.
//
// The original is read from T3_ORACLE_ROOT when set (a checkout), else from this checkout; fixtures are written to this
// checkout. Node must strip TypeScript types (23.6 or later).
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const oracle = (process.env.T3_ORACLE_ROOT ?? root).replace(/\/$/, "");
const fixtures = root + "/rust/crates/server/tests/fixtures/";

const shared = await import(pathToFileURL(oracle + "/packages/shared/src/htmlRender.ts"));
const palettes = await import(pathToFileURL(oracle + "/packages/shared/src/themePalettes.ts"));

const lines = [];
const row = (value) => lines.push(JSON.stringify(value));

// Heights the tools clamp frames to.
for (const input of [
  -5, 0, 79, 79.5, 80, 80.4, 80.5, 99.5, 1999.5, 2000, 2000.5, 5000, 123.49, 123.5,
]) {
  row({ kind: "clamp", input, output: shared.clampHtmlRenderHeight(input) });
}

// The theme a page is handed, with both palettes and both font sets.
for (const [appearance, colors] of [
  ["dark", palettes.T3_CODE_DARK_THEME_COLORS],
  ["light", palettes.T3_CODE_LIGHT_THEME_COLORS],
]) {
  for (const [fonts, set] of [
    ["default", shared.HTML_RENDER_DEFAULT_FONTS],
    ["measure", shared.HTML_RENDER_MEASURE_FONTS],
  ]) {
    const theme = shared.htmlRenderTheme(colors, appearance, set);
    row({
      kind: "theme",
      appearance,
      fonts,
      variables: Object.entries(theme.variables),
      fragment: shared.htmlRenderThemeFragment(theme),
    });
  }
}

// The bootstrap and where it lands.
const astral = "\u{1F600}";
const injections = [
  "<!doctype html><html><head><style>:root{--background:red}</style></head><body>x</body></html>",
  '<meta charset="utf-8"><meta name="viewport" content="width=device-width"><p>hi</p>',
  "",
  "   ",
  "<p>plain</p>",
  "<HEAD><TITLE>T</TITLE></HEAD>",
  "<head lang=en>x</head>",
  "<header>not a head</header>",
  "<html><body>no head</body></html>",
  "<HTML lang=en><BODY>upper</BODY></HTML>",
  "  \n <!DOCTYPE html><body>x</body>",
  "<!doctypehtml><body>x</body>",
  '<!-- copy <head> and <meta name="viewport"> here --><html><head><script>const tag = \'<meta name="viewport">\';</script></head><body>x</body></html>',
  "<!-- unclosed comment <head>",
  "<!--><head>after an empty-looking comment</head>",
  "<!---->\n<head>x</head>",
  ...[
    "textarea",
    "title",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "noscript",
    "plaintext",
    "script",
    "style",
  ].map((tag) => `<${tag}><head><meta name="viewport"></head></${tag}>`),
  '<SCRIPT>var a = "<head>";</SCRIPT ><head>x</head>',
  "<script>unclosed <head>",
  "<script-x><head>x</head>",
  "<styles><head>x</head></styles>",
  "<script2><head>x</head>",
  "<scripts><head>x</head>",
  "<ſcript><head>x</head></ſcript>",
  "<noscript><head>x</head></NoScript\n>after<head>y</head>",
  "<script></script  ><head>x</head>",
  "<script></scriptx><head>x</head>",
  "<script>a</script ><head>x</head>",
  "<script>a</script ><head>x</head>",
  "<script>a</script\u0085><head>x</head>",
  '<template><head><meta name="viewport"></head></template>',
  '<template><template>inner</template><head><meta name="viewport"></head></template>',
  "<template><head>never closed",
  "<template/><head>x</head>",
  "<TEMPLATE id=a><head>x</head></TEMPLATE><head>y</head>",
  "</template><head>stray close</head>",
  "<templates><head>x</head></templates>",
  "<template\n><head>x</head></template>",
  "<template><!-- </template> --><head>x</head></template><head>y</head>",
  '<meta\ncharset=utf-8><meta name = "viewport"><p>spaces</p>',
  "<META CHARSET=utf-8><META NAME=VIEWPORT><p>caps</p>",
  "<meta http-equiv=x content='text/html; charset=utf-8'><p>x</p>",
  "<metacharset><p>not a meta</p>",
  "<meta charset=utf-8><p>nbsp</p>",
  "<meta\u0085charset=utf-8><p>nel</p>",
  "<meta﻿charset=utf-8><p>bom</p>",
  `<p>${"x".repeat(4090)}</p><meta charset=utf-8>`,
  `<p>${"é".repeat(4080)}</p><meta charset=utf-8>`,
  `<p>${astral.repeat(2040)}</p><meta charset=utf-8>`,
  `<p>${"x".repeat(4080)}</p><meta ${"x".repeat(5)} charset=utf-8>`,
  `<!-- ${astral.repeat(2040)} --><meta charset=utf-8><head>x</head>`,
  `<script>${astral.repeat(2040)}</script><meta charset=utf-8>`,
  `<head>é ${astral} ü</head><body>wide ${astral}</body>`,
  "<p>quote \" and ' and ` and   and </p>",
  "<plaintext><head>x",
  "<plaintext2><head>x</head>",
  "<PLAINTEXT><head>x",
  "<p>unterminated <",
  "<",
  "<!",
  "<!-",
  "<!--",
  "<script",
  "<template",
  "<head",
  "<head>",
  "<html>",
  "<html/>",
  "<head/>",
];
for (const html of injections)
  row({ kind: "inject", html, output: shared.injectHtmlRenderBootstrap(html) });
writeFileSync(fixtures + "html-render-shared.jsonl", lines.join("\n") + "\n");

// The address guard: the original's block list and its IPv4-in-IPv6 handling, with this machine's own addresses left
// out so the answers do not depend on where the generator runs.
const proxySource = readFileSync(oracle + "/apps/server/src/htmlRender/publicProxy.ts", "utf8");
const from = proxySource.indexOf("/** This machine and its local networks");
const to = proxySource.indexOf("/**\n * The addresses to connect to for `host`");
if (from < 0 || to < 0) throw new Error("publicProxy.ts has changed shape");
const module = `import * as NodeNet from "node:net";\nconst NodeOS = { networkInterfaces: () => ({}) };\n${stripTypeScriptTypes(
  proxySource.slice(from, to),
)}\nexport { isLocal, embeddedIPv4 };\n`;
const guard = await import("data:text/javascript;base64," + Buffer.from(module).toString("base64"));

const v4 = [
  "0.0.0.0",
  "0.255.255.255",
  "1.0.0.0",
  "1.1.1.1",
  "8.8.8.8",
  "9.255.255.255",
  "10.0.0.0",
  "10.255.255.255",
  "11.0.0.0",
  "100.63.255.255",
  "100.64.0.0",
  "100.100.100.100",
  "100.127.255.255",
  "100.128.0.0",
  "126.255.255.255",
  "127.0.0.0",
  "127.0.0.1",
  "127.255.255.255",
  "128.0.0.0",
  "169.253.255.255",
  "169.254.0.0",
  "169.254.169.254",
  "169.254.255.255",
  "169.255.0.0",
  "172.15.255.255",
  "172.16.0.0",
  "172.31.255.255",
  "172.32.0.0",
  "192.0.0.0",
  "192.0.0.8",
  "192.0.0.170",
  "192.0.0.255",
  "192.0.1.0",
  "192.0.2.1",
  "192.88.99.1",
  "192.167.255.255",
  "192.168.0.0",
  "192.168.255.255",
  "192.169.0.0",
  "198.17.255.255",
  "198.18.0.0",
  "198.19.255.255",
  "198.20.0.0",
  "198.51.100.7",
  "203.0.113.9",
  "223.255.255.255",
  "224.0.0.0",
  "224.0.0.1",
  "239.255.255.255",
  "240.0.0.0",
  "255.255.255.255",
  "93.184.216.34",
  "142.250.80.46",
];
const v6 = [
  "::",
  "::1",
  "::2",
  "::7f00:1",
  "::a00:1",
  "::808:808",
  "::1.2.3.4",
  "::ffff:0:0",
  "::ffff:127.0.0.1",
  "::ffff:7f00:1",
  "::ffff:10.0.0.1",
  "::ffff:a00:1",
  "::ffff:169.254.169.254",
  "::ffff:a9fe:a9fe",
  "::ffff:8.8.8.8",
  "::ffff:808:808",
  "::ffff:0:127.0.0.1",
  "::ffff:0:7f00:1",
  "::ffff:0:8.8.8.8",
  "::ffff:ffff:ffff",
  "::1:0:0:0:0",
  "0:0:0:1::",
  "100::",
  "100::1",
  "2001::",
  "2001::1",
  "2001:0:4136:e378:8000:63bf:3fff:fdd2",
  "2001:1::1",
  "2001:db8::1",
  "2001:4860:4860::8888",
  "2002::",
  "2002:7f00:1::1",
  "2002:a00:1::",
  "2002:a9fe:a9fe::1",
  "2002:808:808::1",
  "2002:c0a8:1::1",
  "2003::1",
  "2606:4700:4700::1111",
  "2a00:1450:4001:81b::200e",
  "64:ff9b::",
  "64:ff9b::7f00:1",
  "64:ff9b::a00:1",
  "64:ff9b::a9fe:a9fe",
  "64:ff9b::c0a8:1",
  "64:ff9b::808:808",
  "64:ff9b::8.8.8.8",
  "64:ff9b::127.0.0.1",
  "64:ff9b:1::",
  "64:ff9b:1::1",
  "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
  "64:ff9b:2::1",
  "64:ff9a::1",
  "fbff::1",
  "fc00::",
  "fc00::1",
  "fd00::1",
  "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
  "fe00::1",
  "fe7f::1",
  "fe80::",
  "fe80::1",
  "febf::1",
  "fec0::1",
  "fedf::1",
  "fef0::1",
  "ff00::",
  "ff02::1",
  "ffff::1",
  "fe80::1%en0",
  "::ffff:127.0.0.1%lo0",
];
const addresses = [];
for (const address of v4) addresses.push({ address, family: 4 });
for (const address of v6) addresses.push({ address, family: 6 });
const guardLines = addresses.map(({ address, family }) =>
  JSON.stringify({ address, family, local: guard.isLocal(address, family) }),
);
writeFileSync(fixtures + "public-proxy-addresses.jsonl", guardLines.join("\n") + "\n");

// The page's local images: the original's matcher and its image recogniser, taken from its own source.
const renderSource = readFileSync(oracle + "/apps/server/src/htmlRender/HtmlRender.ts", "utf8");
const imagesFrom = renderSource.indexOf("const IMAGE_MIME_TYPES");
const imagesTo = renderSource.indexOf("/** Replaces every local image reference");
if (imagesFrom < 0 || imagesTo < 0) throw new Error("HtmlRender.ts has changed shape");
const images = await import(
  "data:text/javascript;base64," +
    Buffer.from(
      `${stripTypeScriptTypes(renderSource.slice(imagesFrom, imagesTo))}\nexport { findLocalImages, isImageBytes, filePathFor, dataUriPrefix };\n`,
    ).toString("base64")
);
const imageLines = [];
const long = (n) => "a".repeat(n);
const findCorpus = [
  '<img src="/abs/shot.png"><div style="background:url(/abs/bg.webp)"></div><script>const shots = [\'/abs/a.jpg\', `/abs/b.svg`];</script>',
  '<img src="https://example.com/a.png"><img src="//cdn.example.com/b.png"><img src="./c.png"><img src="data:image/png;base64,AAAA">',
  '<img src="/a.PNG"><img src="/b.JpEg"><img src="/c.Svg"><img src="/d.ICO">',
  '<img src="/a.png?x=1"><img src="/a.pngx"><img src="/a.pn"><img src="/a.">',
  '<img src="/with space/a.png"><img src=\'/with "double"/a.png\'><img src="/with \'single\'/a.png">',
  '<img src="/line\nbreak.png"><img src="/line\rbreak.png"><img src="/ok.png">',
  '<img src="C:\\Users\\me\\shot.webp"><img src="c:/Users/me/shot.avif"><img src="Z:\\a.bmp"><img src="1:\\a.png"><img src="C:a.png">',
  "<script>const p = 'C:\\\\Users\\\\me\\\\x.png';</script>",
  '"/a.png""/b.png"\'/c.png\'`/d.png`',
  "\"/a.png'",
  '"/unterminated.png',
  "\"/.png\"'/..png'",
  '"/"',
  '"//a.png"',
  '"/a//b.png"',
  `"/${long(2040)}.png"`,
  `"/${long(2048)}.png"`,
  `"/${long(2049)}.png"`,
  `"/${"é".repeat(2048)}.png"`,
  `"/${"é".repeat(2049)}.png"`,
  `"/${"😀".repeat(1024)}.png"`,
  `"/${"😀".repeat(1025)}.png"`,
  `"/${long(2048)}.png" url(/${long(2048)}.png) url(/${long(2049)}.png)`,
  '<div style="background:url(/abs/bg.webp)">',
  "url( /a.png )url(\t/b.png\n)URL(/c.png)uRl(/d.png)",
  "url(\u00a0/a.png\u00a0)url(\ufeff/b.png\ufeff)url(\u2028/c.png\u2029)url(\u0085/d.png)",
  "url(\"/quoted.png\")url('/single.png')url(`/tick.png`)",
  "url(/a.png",
  "url(/a.png.png)url(/b.svg.svg )url(/c.png.txt)",
  "url(/a b.png)url(/c.png d)",
  "url(/a(b).png)url(/c).png",
  "url(//a.png)url(/b.png)",
  "url(C:\\a\\b.png)url(c:/a/b.png)url(C:a.png)",
  "url(/a.png)/b.png)url(/c.png )",
  "url (/a.png)",
  "xurl(/a.png)",
  "URL(/a.png )url(/b.png\n\n)",
  `url(/${long(2048)}.png)`,
  `url(/${long(2049)}.png)`,
  "url(/é.png)url(/😀.png)",
  "ünïcode \"/ünï/a.png\" and 😀 '/b.svg'",
  "'/a.png\u2028.png'",
  '"/a.png"\'/b.png"\'',
  "</script><script>'/abs/x.png'</script>",
  "",
  '"',
  "url(",
  "u",
];
for (const html of findCorpus) {
  const found = images.findLocalImages(html);
  let marked = "";
  let cursor = 0;
  for (const { start, end, path } of found) {
    marked += html.slice(cursor, start) + "<<" + path + ">>";
    cursor = end;
  }
  marked += html.slice(cursor);
  imageLines.push(
    JSON.stringify({ kind: "find", html, paths: found.map((reference) => reference.path), marked }),
  );
}
const bytes = (...values) => Buffer.from(values);
const text = (value) => Buffer.from(value, "utf8");
const imageCorpus = [
  bytes(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a),
  bytes(0x89, 0x50, 0x4e),
  bytes(0xff, 0xd8, 0xff, 0xe0),
  bytes(0xff, 0xd8),
  text("GIF89a"),
  text("GIF87a"),
  text("GIF"),
  bytes(0, 0, 1, 0, 1, 0),
  bytes(0, 0, 1, 0),
  bytes(0, 0, 2, 0),
  Buffer.concat([text("BM"), bytes(1, 2, 3, 4), bytes(0, 0, 0, 0), bytes(9, 9)]),
  Buffer.concat([text("BM"), bytes(1, 2, 3, 4), bytes(0, 0, 0, 1), bytes(9, 9)]),
  text("BM"),
  Buffer.concat([text("RIFF"), bytes(1, 2, 3, 4), text("WEBP")]),
  Buffer.concat([text("RIFF"), bytes(1, 2, 3, 4), text("WEBQ")]),
  Buffer.concat([text("RIFF"), bytes(1, 2, 3, 4)]),
  Buffer.concat([bytes(0, 0, 0, 24), text("ftypavif")]),
  Buffer.concat([bytes(0, 0, 0, 24), text("ftypavis")]),
  Buffer.concat([bytes(0, 0, 0, 24), text("ftypmif1")]),
  Buffer.concat([bytes(0, 0, 0, 24), text("ftypheic")]),
  Buffer.concat([bytes(0, 0, 0, 24), text("ftyp")]),
  Buffer.concat([bytes(0, 0, 0, 24), text("ftypmif1extra")]),
  text("<svg/>"),
  text("<svg xmlns='http://www.w3.org/2000/svg'/>"),
  text("<svg>"),
  text("<svg"),
  text("<svg\t/>"),
  text("<svg\n/>"),
  text("<svg\r/>"),
  text("<svg>x"),
  text("<svgx/>"),
  text("<svg\u00e9/>"),
  text("<SVG/>"),
  text("<Svg/>"),
  text('<?xml version="1.0"?>\n<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"/>'),
  text(
    '<?xml version="1.0"?>\n<?xml-stylesheet href="theme.css"?>\n<!DOCTYPE svg [ <!ENTITY fill "red"> ]>\n<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"/>',
  ),
  text("\ufeff<svg/>"),
  text("  \n\t<svg/>"),
  text("\u00a0<svg/>"),
  text("\u2028<svg/>"),
  text("\u0085<svg/>"),
  text("<!-- comment --><svg/>"),
  text("<!-- unclosed <svg/>"),
  text("<!--x--><!-- y --><svg/>"),
  text("<!doctype html><body><svg></svg>API_KEY=abc123"),
  text('<!DOCTYPE config [<!ENTITY a "a"><!ENTITY b "]><svg/>">]><config>API_KEY=abc123</config>'),
  text(`${"<?p?>".repeat(40)}<config><svg/></config>`),
  text('<!DOCTYPE svg [<!ENTITY a "x><svg/>'),
  text("<!DOCTYPE svg [ <!-- ] --> <?pi ] ?> ]><svg/>"),
  text("<!DOCTYPE svg [ <!-- ] ><svg/>"),
  text("<!DOCTYPE svg><svg/>"),
  text("<!doctype svg><svg/>"),
  text("<!DocType svg ><svg/>"),
  text("<!DOCTYPE"),
  text("<!doctype 'a>b' ><svg/>"),
  text("<?xml"),
  text("<?xml ?>"),
  text("<?xml ?><?xml ?><svg/>"),
  text("API_KEY=abc123"),
  text(""),
  text("<html><svg/></html>"),
  Buffer.concat([text("<svg/>"), Buffer.alloc(5000, 0x61)]),
  Buffer.concat([Buffer.alloc(4090, 0x20), text("<svg/>")]),
  Buffer.concat([Buffer.alloc(4095, 0x20), text("<svg/>")]),
  Buffer.concat([text("<!-- "), Buffer.alloc(4100, 0x20), text("--><svg/>")]),
  Buffer.concat([text("<svg/>"), bytes(0xff, 0xfe, 0xfd)]),
  Buffer.concat([bytes(0xc3), text("<svg/>")]),
  Buffer.concat([bytes(0xef, 0xbb, 0xbf), text("<svg/>")]),
  Buffer.concat([Buffer.alloc(4094, 0x20), Buffer.from("é")]),
];
for (const body of imageCorpus) {
  imageLines.push(
    JSON.stringify({
      kind: "image",
      bytes: body.toString("base64"),
      image: images.isImageBytes(new Uint8Array(body)),
    }),
  );
}
for (const path of [
  "/a/b.png",
  "/a/b.PNG",
  "/a/b.JpG",
  "/a/b.svg",
  "/a/b.ico",
  "/a/b",
  "/a/b.tar.gz",
  "/a.b/c",
  "C:\\x\\y.WEBP",
  "/a/b.avif",
  "x.bmp",
]) {
  imageLines.push(JSON.stringify({ kind: "mime", path, prefix: images.dataUriPrefix(path) }));
}
for (const path of [
  "C:\\\\Users\\\\me\\\\x.png",
  "C:\\Users\\me\\x.png",
  "/a\\\\b.png",
  "c:\\\\\\\\x.png",
  "1:\\\\x.png",
  ":\\\\x.png",
  "/plain.png",
  "",
]) {
  imageLines.push(JSON.stringify({ kind: "file", path, file: images.filePathFor(path) }));
}
// Which provider spellings of a tool name are `html_render`: the pages a thread published are found by this when it is deleted.
const presentation = await import(
  pathToFileURL(oracle + "/packages/shared/src/t3McpToolPresentation.ts")
);
const toolNames = [
  "html_render",
  "html_preview",
  "mcp__t3-code__html_render",
  "mcp__T3-CODE__html_render",
  "mcp__t3_code__html_render",
  "mcp__t3code__html_render",
  "mcp__other__html_render",
  "mcp__t3-code__html_renderx",
  "mcp__t3-code__HTML_RENDER",
  "mcp__t3-code__html_preview",
  "mcp____html_render",
  "mcp__t3-code__",
  "mcp__a__b__html_render",
  "mcp__t3-code__t3-code__html_render",
  "mcp__t3-code__x__html_render",
  "t3-code.html_render",
  "T3-Code.html_render",
  "t3_code:html_render",
  "t3code/html_render",
  "t3-code \u00b7 html_render",
  "t3-code\u00b7html_render",
  "t3-code .html_render",
  "t3-code.",
  "t3-code.html_render.x",
  "t3-code-thread_1_html_render",
  "t3-code-thread_html_render_html_render",
  "t3-code-thread_1_html_preview",
  "t3-code-html_render",
  "t3-code_html_render",
  "t3-code html_render",
  "t3-code__html_render",
  "mcp_t3-code_html_render",
  "mcp-t3code.html_render",
  "mcp__t3code_html_render",
  "mcp-t3-code-html_render",
  "t3code-html_render",
  "t3 code html_render",
  "t3codehtml_render",
  "t3-codehtml_render",
  "t3-code:html_render complete",
  "t3-code.html_render completed",
  "t3-code.html_render  Complete  ",
  "  html_render  ",
  "html_render complete",
  "html_render Completed",
  "html_render completeness",
  "xt3-code.html_render",
  "t3-codex.html_render",
  "mcp__t3-code__html_render\n",
  "mcp__t3-code__html\nrender",
  "mcp__t3\n-code__html_render",
  "",
  "mcp__",
  "mcp__a__",
  "t3-code-",
  "_html_render",
  "other_html_render",
  "t3-code-x_other_html_render",
  "html-render",
  "htmlrender",
  "dynamic_tool",
  "Read",
  "mcp__codex_apps__html_render",
  "functions.html_render",
  "t3-code.mcp__t3-code__html_render",
];
for (const name of toolNames) {
  imageLines.push(
    JSON.stringify({
      kind: "tool",
      name,
      htmlRender: presentation.resolveT3McpToolId(name) === "html_render",
    }),
  );
}
writeFileSync(fixtures + "html-render-images.jsonl", imageLines.join("\n") + "\n");
console.log(
  `wrote ${lines.length} helper cases, ${guardLines.length} addresses and ${imageLines.length} image cases`,
);
