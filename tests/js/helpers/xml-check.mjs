// Tiny well-formedness check for the XHTML/XUL files (no XML parser ships with Node). Handles comments, processing
// instructions, CDATA, attributes with '>' inside quotes, self-closing tags and multiple roots (pane fragments).
// Returns an array of problems (empty = well formed). Owner: js-shell.
export function checkXml(text) {
  const problems = [];
  const stack = [];
  let i = 0;
  const n = text.length;
  while (i < n) {
    const lt = text.indexOf('<', i);
    if (lt === -1) break;
    if (text.startsWith('<!--', lt)) {
      const end = text.indexOf('-->', lt + 4);
      if (end === -1) { problems.push('unterminated comment'); break; }
      i = end + 3; continue;
    }
    if (text.startsWith('<![CDATA[', lt)) {
      const end = text.indexOf(']]>', lt);
      if (end === -1) { problems.push('unterminated CDATA'); break; }
      i = end + 3; continue;
    }
    if (text.startsWith('<?', lt)) {
      const end = text.indexOf('?>', lt);
      if (end === -1) { problems.push('unterminated processing instruction'); break; }
      i = end + 2; continue;
    }
    // find the end of the tag, skipping quoted attribute values
    let j = lt + 1;
    let quote = null;
    for (; j < n; j++) {
      const c = text[j];
      if (quote) { if (c === quote) quote = null; continue; }
      if (c === '"' || c === "'") { quote = c; continue; }
      if (c === '>') break;
    }
    if (j >= n) { problems.push('unterminated tag at offset ' + lt); break; }
    const body = text.slice(lt + 1, j);
    i = j + 1;
    if (body.startsWith('/')) {
      const name = body.slice(1).trim();
      const open = stack.pop();
      if (open !== name) problems.push(`closing </${name}> does not match <${open}>`);
    } else {
      const name = body.match(/^[^\s/>]+/)?.[0];
      if (!name) { problems.push('empty tag name at offset ' + lt); continue; }
      if (!body.endsWith('/')) stack.push(name);
    }
  }
  for (const open of stack) problems.push(`unclosed <${open}>`);
  return problems;
}
