// Entry — reads `data` and renders the page. No copy is hard-coded here;
// every visible string flows through src/data.js.

import './style.css';
import { data } from './data.js';

// ---------- tiny DOM helper ----------
const h = (tag, attrs = {}, ...kids) => {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'html') el.innerHTML = v;
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2).toLowerCase(), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return el;
};

// Render a string with `backticked` spans as inline <code>.
const richText = (str) => {
  const parts = String(str).split(/(`[^`]+`)/g);
  return parts.map(p =>
    p.startsWith('`') && p.endsWith('`')
      ? h('code', null, p.slice(1, -1))
      : p,
  );
};

// ---------- sections ----------

function Nav({ brand, brandTag, links }) {
  return h('header', { class: 'nav' },
    h('div', { class: 'container nav-inner' },
      h('a', { href: '#', class: 'nav-brand' },
        h('span', null, h('span', { class: 'dot' }), brand),
        h('span', { class: 'tag' }, brandTag),
      ),
      h('nav', { class: 'nav-links' },
        ...links.map(l => h('a', {
          href: l.href,
          class: l.external ? 'external' : null,
          target: l.external ? '_blank' : null,
          rel: l.external ? 'noopener' : null,
        }, l.label)),
      ),
    ),
  );
}

function Hero({ eyebrow, title, body, install, stats }) {
  const copyBtn = h('button', { class: 'copy', type: 'button' }, install.copyLabel);
  copyBtn.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(install.command);
      copyBtn.textContent = install.copiedLabel;
      copyBtn.classList.add('copied');
      setTimeout(() => {
        copyBtn.textContent = install.copyLabel;
        copyBtn.classList.remove('copied');
      }, 1600);
    } catch {}
  });

  return h('section', { class: 'hero' },
    h('div', { class: 'container' },
      h('div', { class: 'eyebrow' }, eyebrow),
      h('h1', null, ...title.map(t => h('span', { class: 'line' }, t))),
      h('p', { class: 'hero-body', html: body }),
      h('div', { class: 'install-box' },
        h('span', { class: 'label' }, install.label),
        h('code', { class: 'cmd' }, install.command),
        copyBtn,
      ),
      h('div', { class: 'hero-stats' },
        ...stats.map(s => h('div', { class: 'stat' },
          h('div', { class: 'v' }, s.value),
          h('div', { class: 'l' }, s.label),
        )),
      ),
    ),
  );
}

function Pitch({ id, eyebrow, title, body, flows, blocked, table }) {
  return h('section', { id },
    h('div', { class: 'container' },
      h('div', { class: 'eyebrow' }, eyebrow),
      h('h2', null, title),
      h('p', { class: 'lead' }, body),

      h('div', { class: 'flows' },
        ...flows.map(f => h('div', { class: `flow ${f.kind}` },
          h('div', { class: 'label' }, f.label),
          h('div', { class: 'flow-steps' },
            ...f.steps.flatMap((s, i) => [
              h('span', { class: 'step' }, s),
              i < f.steps.length - 1 ? h('span', { class: 'arrow' }, '→') : null,
            ]).filter(Boolean),
          ),
          h('div', { class: 'verdict' }, f.verdict),
        )),
      ),

      h('div', { class: 'blocked' },
        h('div', { class: 'head' }, blocked.label),
        h('pre', null, blocked.lines.join('\n')),
      ),

      h('table', { class: 'compare' },
        h('thead', null, h('tr', null, ...table.head.map(c => h('th', null, c)))),
        h('tbody', null, ...table.rows.map(r => h('tr', null, ...r.map(c => h('td', null, c))))),
      ),
    ),
  );
}

function MarkerDemo({ id, eyebrow, title, body, file, legend }) {
  // Render a marker line with agent-coloured agentId.
  const markerLine = (text, agent) => {
    // Highlight `agent=<id>` in colour. Cheap regex.
    const html = text.replace(
      /agent=([^\s]+)/,
      `agent=<span class="agent-${agent.toLowerCase()}">$1</span>`,
    );
    return h('span', { class: 'ln marker', html });
  };
  const codeLine = (text, tag) =>
    h('span', { class: `ln code ${tag ? `tag-${tag}` : ''}` }, text || ' ');

  return h('section', { id },
    h('div', { class: 'container' },
      h('div', { class: 'eyebrow' }, eyebrow),
      h('h2', null, title),
      h('p', { class: 'lead' }, body),

      h('div', { class: 'file' },
        h('div', { class: 'titlebar' },
          h('span', { class: 'traffic' }, h('span'), h('span'), h('span')),
          h('span', { class: 'filename' }, file.name),
        ),
        h('div', { class: 'body' },
          h('div', { class: 'gutter' },
            ...file.lines.map((_, i) => h('div', null, String(i + 1))),
          ),
          h('div', { class: 'lines' },
            ...file.lines.map(l =>
              l.kind === 'marker' ? markerLine(l.text, l.agent) : codeLine(l.text, l.tag),
            ),
          ),
        ),
      ),

      h('div', { class: 'legend' },
        ...legend.map(item => h('div', { class: 'item' },
          h('span', { class: `swatch ${item.swatch}` }),
          item.label,
        )),
      ),
    ),
  );
}

function Architecture({ id, eyebrow, title, body, layers, sidecar }) {
  return h('section', { id },
    h('div', { class: 'container' },
      h('div', { class: 'eyebrow' }, eyebrow),
      h('h2', null, title),
      h('p', { class: 'lead' }, body),

      h('div', { class: 'stack' },
        ...layers.map(l => h('div', { class: 'layer' },
          h('div', { class: 'name' }, l.name),
          h('div', { class: 'path' }, l.path),
          h('div', { class: 'role' }, l.role),
        )),
      ),

      h('div', { class: 'sidecar' },
        h('div', { class: 'name' }, sidecar.name),
        h('div', { class: 'path' }, sidecar.path),
        h('div', { class: 'role' }, sidecar.role),
      ),
    ),
  );
}

function Design({ id, eyebrow, title, body, principles, lifecycle }) {
  return h('section', { id },
    h('div', { class: 'container' },
      h('div', { class: 'eyebrow' }, eyebrow),
      h('h2', null, title),
      h('p', { class: 'lead' }, ...richText(body)),

      h('div', { class: 'principles' },
        ...principles.map(p => h('div', { class: 'principle' },
          h('div', { class: 'name' }, p.name),
          h('div', { class: 'role' }, ...richText(p.body)),
        )),
      ),

      h('div', { class: 'lifecycle' },
        h('div', { class: 'head' }, lifecycle.label),
        h('pre', null, lifecycle.lines.join('\n')),
      ),
    ),
  );
}

function Schema({ id, eyebrow, title, body, file, fields, stale }) {
  return h('section', { id },
    h('div', { class: 'container' },
      h('div', { class: 'eyebrow' }, eyebrow),
      h('h2', null, title),
      h('p', { class: 'lead' }, ...richText(body)),

      h('div', { class: 'file schema-file' },
        h('div', { class: 'titlebar' },
          h('span', { class: 'traffic' }, h('span'), h('span'), h('span')),
          h('span', { class: 'filename' }, file.name),
        ),
        h('div', { class: 'body' },
          h('div', { class: 'gutter' },
            ...file.lines.map((_, i) => h('div', null, String(i + 1))),
          ),
          h('div', { class: 'lines' },
            ...file.lines.map(l => h('span', { class: 'ln code' }, l || ' ')),
          ),
        ),
      ),

      h('div', { class: 'fields' },
        ...fields.map(f => h('div', { class: 'field-row' },
          h('div', { class: 'field-name' },
            h('code', null, f.name),
            h('span', { class: 'field-type' }, f.type),
          ),
          h('div', { class: 'field-role' }, ...richText(f.role)),
        )),
      ),

      h('div', { class: 'stale' },
        h('div', { class: 'head' }, stale.label),
        h('ul', null, ...stale.rules.map(r => h('li', null, ...richText(r)))),
      ),
    ),
  );
}

function Install({ id, eyebrow, title, steps, requirements }) {
  return h('section', { id },
    h('div', { class: 'container' },
      h('div', { class: 'eyebrow' }, eyebrow),
      h('h2', null, title),
      h('div', { class: 'steps' },
        ...steps.map(s => h('div', { class: 'step-row' },
          h('div', { class: 'n' }, s.n),
          h('div', null,
            h('div', { class: 'label' }, s.label),
            h('pre', { class: 'cmd' }, s.command),
            h('div', { class: 'note' }, s.note),
          ),
        )),
      ),
      h('div', { class: 'req' }, ...requirements.map(r => h('span', null, r))),
    ),
  );
}

function Footer({ text, link }) {
  return h('footer', null,
    h('div', { class: 'container row' },
      h('span', null, text),
      h('a', { href: link.href, target: '_blank', rel: 'noopener' }, link.label),
    ),
  );
}

// ---------- mount ----------

document.title = data.meta.title;
const meta = document.querySelector('meta[name="description"]')
  || document.head.appendChild(Object.assign(document.createElement('meta'), { name: 'description' }));
meta.setAttribute('content', data.meta.description);

const app = document.getElementById('app');
app.append(
  Nav(data.nav),
  Hero(data.hero),
  Pitch(data.pitch),
  MarkerDemo(data.marker),
  Architecture(data.architecture),
  Design(data.design),
  Schema(data.schema),
  Install(data.install),
  Footer(data.footer),
);
