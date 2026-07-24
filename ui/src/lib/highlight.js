// A small, dependency-free syntax tokenizer.
//
// It returns TOKENS, never markup: the renderer turns them into React
// elements, so highlighted model output still cannot inject anything. A
// highlighter library would be the obvious alternative, but the dashboard
// ships as one inlined HTML file and the languages that actually show up in
// traces (JSON above all, then SQL and the usual scripting languages) are
// covered by one generic scanner.
//
// Token types: keyword, string, number, comment, property, punct, plain.

const WORD = /[A-Za-z_$@][\w$]*/y;
const NUMBER = /(?:0[xXbBoO][0-9a-fA-F_]+|\d[\d_]*(?:\.[\d_]+)?(?:[eE][+-]?\d+)?)/y;

function words(list) {
  return new Set(list.split(/\s+/).filter(Boolean));
}

const JS_KEYWORDS = words(`
  async await break case catch class const continue debugger default delete do else export extends
  finally for from function get if import in instanceof let new of return set static super switch
  this throw try typeof var void while with yield true false null undefined
  string number boolean any unknown never interface type enum implements readonly public private protected`);

const PY_KEYWORDS = words(`
  and as assert async await break class continue def del elif else except finally for from global
  if import in is lambda nonlocal not or pass raise return try while with yield True False None self`);

const SQL_KEYWORDS = words(`
  select from where group by order having limit offset join inner left right full outer on as and or
  not null is in like between union all distinct insert into values update set delete create table
  alter drop index view with case when then else end asc desc count sum avg min max cast interval
  now current_date current_timestamp date_trunc primary key foreign references default constraint`);

const RUST_KEYWORDS = words(`
  as async await break const continue crate dyn else enum extern false fn for if impl in let loop
  match mod move mut pub ref return self Self static struct super trait true type unsafe use where while`);

const GO_KEYWORDS = words(`
  break case chan const continue default defer else fallthrough for func go goto if import interface
  map package range return select struct switch type var true false nil`);

const SHELL_KEYWORDS = words(`
  if then else elif fi for while do done case esac function in return export local readonly set unset
  echo cd exit source alias sudo curl grep awk sed cat ls mkdir rm cp mv`);

const GENERIC = {
  lineComments: ['//', '#'],
  blockComment: ['/*', '*/'],
  quotes: ['"', "'", '`'],
  keywords: new Set(),
};

const LANGUAGES = {
  json: { lineComments: [], blockComment: null, quotes: ['"'], keywords: words('true false null'), jsonProperty: true },
  javascript: { lineComments: ['//'], blockComment: ['/*', '*/'], quotes: ['"', "'", '`'], keywords: JS_KEYWORDS },
  python: { lineComments: ['#'], blockComment: null, quotes: ['"', "'"], keywords: PY_KEYWORDS, triple: true },
  sql: { lineComments: ['--'], blockComment: ['/*', '*/'], quotes: ["'", '"'], keywords: SQL_KEYWORDS, caseInsensitive: true },
  rust: { lineComments: ['//'], blockComment: ['/*', '*/'], quotes: ['"'], keywords: RUST_KEYWORDS },
  go: { lineComments: ['//'], blockComment: ['/*', '*/'], quotes: ['"', '`'], keywords: GO_KEYWORDS },
  shell: { lineComments: ['#'], blockComment: null, quotes: ['"', "'"], keywords: SHELL_KEYWORDS },
  yaml: { lineComments: ['#'], blockComment: null, quotes: ['"', "'"], keywords: words('true false null yes no on off'), jsonProperty: true },
};

const ALIASES = {
  js: 'javascript', jsx: 'javascript', ts: 'javascript', tsx: 'javascript', typescript: 'javascript',
  mjs: 'javascript', node: 'javascript',
  py: 'python', python3: 'python',
  sh: 'shell', bash: 'shell', zsh: 'shell', console: 'shell', shellsession: 'shell',
  yml: 'yaml', rs: 'rust', golang: 'go', postgresql: 'sql', psql: 'sql', mysql: 'sql',
};

/** Canonical language id for a fence info string, or null when unknown. */
export function normalizeLanguage(language) {
  if (typeof language !== 'string') return null;
  const key = language.trim().toLowerCase().split(/[\s:,]/)[0];
  if (!key) return null;
  return LANGUAGES[key] ? key : (ALIASES[key] || null);
}

// Highlighting a novel-sized blob is wasted work; the reader is scrolling, not
// studying it.
const MAX_HIGHLIGHT_CHARS = 120000;

/** Tokenizes `code`. Unknown languages still get strings, numbers and
    comments, which is most of the readability win. */
export function tokenize(code, language) {
  const text = String(code ?? '');
  if (!text) return [];
  if (text.length > MAX_HIGHLIGHT_CHARS) return [{ text, type: 'plain' }];
  const id = normalizeLanguage(language);
  const config = id ? LANGUAGES[id] : GENERIC;
  const tokens = [];
  let plain = '';
  const flush = () => { if (plain) { tokens.push({ text: plain, type: 'plain' }); plain = ''; } };
  const push = (value, type) => { flush(); tokens.push({ text: value, type }); };

  let i = 0;
  while (i < text.length) {
    const rest = text.slice(i);

    // Comments.
    const line = (config.lineComments || []).find((marker) => rest.startsWith(marker));
    if (line) {
      const end = text.indexOf('\n', i);
      const stop = end === -1 ? text.length : end;
      push(text.slice(i, stop), 'comment');
      i = stop;
      continue;
    }
    if (config.blockComment && rest.startsWith(config.blockComment[0])) {
      const close = text.indexOf(config.blockComment[1], i + config.blockComment[0].length);
      const stop = close === -1 ? text.length : close + config.blockComment[1].length;
      push(text.slice(i, stop), 'comment');
      i = stop;
      continue;
    }

    // Strings, including Python triple quotes.
    const quote = (config.quotes || []).find((q) => rest.startsWith(q));
    if (quote) {
      const triple = config.triple && (rest.startsWith(quote.repeat(3)));
      const delimiter = triple ? quote.repeat(3) : quote;
      let j = i + delimiter.length;
      while (j < text.length) {
        if (text[j] === '\\') { j += 2; continue; }
        if (text.startsWith(delimiter, j)) { j += delimiter.length; break; }
        if (!triple && text[j] === '\n') break; // unterminated: stop at the line
        j += 1;
      }
      const value = text.slice(i, Math.min(j, text.length));
      // A JSON/YAML key is a string followed by a colon.
      let type = 'string';
      if (config.jsonProperty) {
        const after = text.slice(j).match(/^\s*:/);
        if (after) type = 'property';
      }
      push(value, type);
      i += value.length;
      continue;
    }

    // Numbers.
    NUMBER.lastIndex = i;
    const number = NUMBER.exec(text);
    if (number && number.index === i && !/[\w$]/.test(text[i - 1] || '')) {
      push(number[0], 'number');
      i += number[0].length;
      continue;
    }

    // Words: keyword or plain.
    WORD.lastIndex = i;
    const word = WORD.exec(text);
    if (word && word.index === i) {
      const value = word[0];
      const probe = config.caseInsensitive ? value.toLowerCase() : value;
      if (config.keywords && config.keywords.has(probe)) push(value, 'keyword');
      else plain += value;
      i += value.length;
      continue;
    }

    const char = text[i];
    if ('{}[]()<>;:,.=+-*/%!&|^~?'.includes(char)) push(char, 'punct');
    else plain += char;
    i += 1;
  }
  flush();
  return tokens;
}
