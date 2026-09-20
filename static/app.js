let state;
let selectedHypothesis = 'H-A';
let selectedSet = null;
let liveResult = null;

async function post(path, payload) {
  const response = await fetch(path, { method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(payload) });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || response.statusText);
  await refresh();
  return data;
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function fmt(value, digits=3) {
  return value === null || value === undefined ? '—' : Number(value).toFixed(digits);
}

async function refresh() {
  state = await fetch('/api/state').then(r => r.json());
  if (!state.hypotheses.some(h => h.hypothesis_id === selectedHypothesis)) selectedHypothesis = state.hypotheses[0]?.hypothesis_id;
  liveResult = state.current[selectedHypothesis] || null;
  selectedSet = state.hypotheses.find(h => h.hypothesis_id === selectedHypothesis)?.current_set_id || null;
  renderTabs();
  renderSets();
  renderEvents(liveResult);
  renderPicks();
  renderRejected(liveResult);
  renderLog();
  document.getElementById('raw').textContent = JSON.stringify(state, null, 2);
}

function renderTabs() {
  const tabs = document.getElementById('tabs');
  tabs.replaceChildren();
  for (const hypothesis of state.hypotheses) {
    const button = el('button', hypothesis.hypothesis_id === selectedHypothesis ? 'primary' : '', `${hypothesis.hypothesis_id} · ${hypothesis.correction_version} · #${hypothesis.current_set_id || '—'}`);
    button.title = hypothesis.name;
    button.onclick = () => { selectedHypothesis = hypothesis.hypothesis_id; refresh(); };
    tabs.appendChild(button);
  }
}

async function renderSets() {
  const select = document.getElementById('sets');
  const data = await fetch('/api/sets').then(r => r.json());
  select.replaceChildren();
  for (const set of data.sets || []) {
    const option = el('option', null, `${set.set_id} ${set.hypothesis_id} ${set.correction_version}`);
    option.value = set.set_id;
    if (set.set_id === selectedSet) option.selected = true;
    select.appendChild(option);
  }
}

function renderEvents(result) {
  const root = document.getElementById('events');
  root.replaceChildren();
  if (!result) { root.textContent = '尚无候选集'; return; }
  for (const event of result.events || []) {
    const card = el('div', 'card');
    const head = el('h3', null, `${event.label} / ${event.event_key}`);
    card.appendChild(head);
    card.appendChild(el('div', 'muted', `台站 ${event.station_count} · 拾取 ${event.pick_count} · 方位空隙 ${fmt(event.azimuthal_gap_deg,1)}° · P=${event.phase_counts.P||0}, S=${event.phase_counts.S||0}`));
    for (const candidate of event.candidates) {
      const block = el('details');
      const summary = el('summary', null, `#${candidate.rank} ${candidate.status} score=${fmt(candidate.score,4)} RMS=${fmt(candidate.rms_residual_s,3)}s depth=${fmt(candidate.depth_m,0)}m origin=${fmt(candidate.origin_time,3)}s`);
      summary.className = `status-${candidate.status}`;
      block.appendChild(summary);
      block.appendChild(el('div', 'muted', `lat ${fmt(candidate.latitude,6)}  lon ${fmt(candidate.longitude,6)}  x/y ${fmt(candidate.x_m,0)} / ${fmt(candidate.y_m,0)} m`));
      block.appendChild(el('div', null, `协方差 rank ${candidate.covariance.rank}/${candidate.covariance.parameter_count}, PD=${candidate.covariance.positive_definite}, σ x/y/z/t = ${fmt(candidate.covariance.source_sigma.x_m,0)}, ${fmt(candidate.covariance.source_sigma.y_m,0)}, ${fmt(candidate.covariance.source_sigma.depth_m,0)} m, ${fmt(candidate.covariance.source_sigma.origin_time_s,3)} s`));
      const warnings = el('div', 'muted', (candidate.warnings || []).join('；') || '无数值或几何警告');
      block.appendChild(warnings);
      const table = el('table');
      table.innerHTML = '<thead><tr><th>拾取</th><th>台站</th><th>相位</th><th>到时残差(s)</th><th>权重</th><th>贡献</th><th></th></tr></thead>';
      const body = el('tbody');
      for (const a of candidate.assignments) {
        const tr = el('tr');
        [a.observation_id, a.station_id, a.phase, fmt(a.residual_s,3), fmt(a.weight,3), fmt(a.score_contribution,3), a.locked ? 'locked' : ''].forEach(v => tr.appendChild(el('td', null, v)));
        body.appendChild(tr);
      }
      table.appendChild(body);
      block.appendChild(table);
      card.appendChild(block);
    }
    root.appendChild(card);
  }
}

function renderPicks() {
  const root = document.getElementById('picks');
  root.replaceChildren();
  const locked = new Map(Object.entries(state.hypotheses.find(h => h.hypothesis_id === selectedHypothesis)?.locked || {}));
  const table = el('table');
  table.innerHTML = '<thead><tr><th>ID</th><th>台站</th><th>相位</th><th>原始时间(s)</th><th>σ(s)</th><th>置信度</th><th>来源</th><th>状态</th><th>操作</th></tr></thead>';
  const body = el('tbody');
  for (const pick of state.picks) {
    const tr = el('tr');
    const status = [pick.superseded ? 'superseded' : '', pick.noise ? 'noise' : '', locked.has(pick.observation_id) ? 'locked' : ''].filter(Boolean).join(',');
    [pick.observation_id, pick.station_id, pick.phase, fmt(pick.raw_time,3), fmt(pick.time_uncertainty_s,3), fmt(pick.confidence,2), pick.source, status].forEach(v => tr.appendChild(el('td', null, v)));
    const actions = el('td');
    const lockButton = el('button', null, locked.has(pick.observation_id) ? '解锁' : '锁定');
    lockButton.onclick = async () => {
      try { await post(locked.has(pick.observation_id) ? '/api/unlock' : '/api/lock', { hypothesis_id: selectedHypothesis, observation_id: pick.observation_id }); }
      catch (e) { alert(e.message); }
    };
    const noiseButton = el('button', 'danger', pick.noise ? '取消噪声' : '噪声');
    noiseButton.onclick = async () => {
      try { await post('/api/noise', { observation_id: pick.observation_id, noise: !pick.noise }); }
      catch (e) { alert(e.message); }
    };
    actions.append(lockButton, ' ', noiseButton);
    tr.appendChild(actions);
    body.appendChild(tr);
  }
  table.appendChild(body);
  root.appendChild(table);
}

function renderRejected(result) {
  const root = document.getElementById('rejected');
  root.replaceChildren();
  const rejected = (result?.rejected || []);
  if (!rejected.length) { root.textContent = '无被剔除拾取。'; return; }
  const table = el('table');
  table.innerHTML = '<thead><tr><th>拾取</th><th>台站</th><th>相位</th><th>原因</th></tr></thead>';
  const body = el('tbody');
  for (const item of rejected) {
    const tr = el('tr');
    [item.observation_id, item.station_id, item.phase, item.reason].forEach(v => tr.appendChild(el('td', null, v)));
    body.appendChild(tr);
  }
  table.appendChild(body);
  root.appendChild(table);
}

function renderLog() {
  const root = document.getElementById('log');
  root.replaceChildren();
  const table = el('table');
  table.innerHTML = '<thead><tr><th>#</th><th>时间(s)</th><th>操作</th><th>假设</th><th>载荷/哈希</th></tr></thead>';
  const body = el('tbody');
  for (const log of state.operation_log.slice(-12).reverse()) {
    const tr = el('tr');
    [log.log_id, fmt(log.occurred_at,3), log.operation, log.target_hypothesis, `${log.payload_json}  hash=${log.content_hash.slice(0,12)}`].forEach(v => tr.appendChild(el('td', null, String(v))));
    body.appendChild(tr);
  }
  table.appendChild(body);
  root.appendChild(table);
}

document.getElementById('loadSet').onclick = async () => {
  const id = Number(document.getElementById('sets').value);
  const data = await fetch(`/api/set?id=${id}`).then(r => r.json());
  renderEvents(data.result);
  renderRejected(data.result);
};
document.getElementById('recompute').onclick = async () => {
  try { await post('/api/recompute', { hypothesis_id: selectedHypothesis, correction_version: state.hypotheses.find(h => h.hypothesis_id === selectedHypothesis).correction_version }); }
  catch (e) { alert(e.message); }
};
document.getElementById('newCorrection').onclick = async () => {
  try {
    const version = document.getElementById('correctionName').value.trim() || `v${Date.now()}`;
    const station = document.getElementById('offsetStation').value.trim();
    const offset = Number(document.getElementById('offsetValue').value || '0');
    const payload = { version, parent_version: state.hypotheses.find(h => h.hypothesis_id === selectedHypothesis).correction_version, offsets_s: {}, note: 'browser-created piecewise constant correction' };
    if (station) payload.offsets_s[station] = offset;
    await post('/api/corrections', payload);
    await post('/api/recompute', { hypothesis_id: selectedHypothesis, correction_version: version });
  } catch (e) { alert(e.message); }
};
refresh();
setInterval(refresh, 15000);
