<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { base } from '$app/paths';
  import LoadImageModal from '$lib/LoadImageModal.svelte';
  import BridgeDetail from '$lib/BridgeDetail.svelte';

  let overview: any = $state(null);
  let images: any[] = $state([]);
  let nodes: any[] = $state([]);
  let bridges: any[] = $state([]);
  let connections: any[] = $state([]);
  let settings: any = $state(null);
  let err = $state('');

  let showLoad = $state(false);
  let showNewBridge = $state(false);
  let showSettings = $state(false);
  let newBridgeName = $state('');

  let expandedBridge = $state<number | null>(null);

  let timer: any = null;

  async function refresh() {
    try {
      const [ov, im, nd, br, cn, st] = await Promise.all([
        fetch(`${base}/api/overview`).then(r => r.json()),
        fetch(`${base}/api/images`).then(r => r.json()),
        fetch(`${base}/api/nodes`).then(r => r.json()),
        fetch(`${base}/api/bridges`).then(r => r.json()),
        fetch(`${base}/api/connections`).then(r => r.json()),
        fetch(`${base}/api/settings`).then(r => r.json()),
      ]);
      overview = ov; images = im; nodes = nd; bridges = br; connections = cn; settings = st; err = '';
    } catch { err = 'Cannot reach kdct server'; }
  }

  async function autoCheck() {
    try { await fetch(`${base}/api/auto-check`, { method: 'POST' }); } catch {}
    refresh();
  }

  async function toggleTls(want: boolean) {
    try {
      const res = await fetch(`${base}/api/settings`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ tls_enabled: want }),
      });
      if (!res.ok) {
        const e = await res.json().catch(() => ({}));
        alert(e.error || `Failed to update TLS setting (${res.status})`);
        return;
      }
      refresh();
    } catch { alert('Failed to update TLS setting'); }
  }

  onMount(() => { refresh(); timer = setInterval(autoCheck, 5000); });
  onDestroy(() => { if (timer) clearInterval(timer); });

  function mem(mb: number) { return mb >= 1024 ? `${(mb/1024).toFixed(1)} GB` : `${mb} MB`; }
  function dk(v: string) { return v ? v.split('.').slice(0,2).join('.') : '—'; }
  function ago(ts: number) {
    const s = Math.floor(Date.now()/1000 - ts);
    if (s < 60) return 'just now'; if (s < 3600) return `${Math.floor(s/60)}m ago`;
    if (s < 86400) return `${Math.floor(s/3600)}h ago`; return `${Math.floor(s/86400)}d ago`;
  }

  function toggleBridge(id: number) {
    if (expandedBridge === id) { expandedBridge = null; return; }
    expandedBridge = id;
  }

  async function deleteBridge(id: number) {
    if (!confirm('Delete this bridge?')) return;
    await fetch(`${base}/api/bridges/${id}`, { method: 'DELETE' });
    if (expandedBridge === id) { expandedBridge = null; }
    refresh();
  }

  async function createBridge() {
    if (!newBridgeName) return;
    try {
      await fetch(`${base}/api/bridges`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ name: newBridgeName }) });
      showNewBridge = false; newBridgeName = ''; refresh();
    } catch {}
  }

  async function createConnection() {
    try {
      await fetch(`${base}/api/connections`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ name: 'connection' }) });
      refresh();
    } catch {}
  }

  async function deleteConnection(id: number) {
    if (!confirm('Delete this connection?')) return;
    await fetch(`${base}/api/connections/${id}`, { method: 'DELETE' });
    refresh();
  }

  async function updateConnection(id: number, field: string, value: number | null) {
    const body: any = {};
    body[field] = value;
    await fetch(`${base}/api/connections/${id}`, { method: 'PATCH', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    refresh();
  }

  const online = $derived(nodes.filter((n: any) => n.status === 'online'));
</script>

{#if err}
  <div class="msg err">{err}</div>
{/if}

<div class="page">
  <div class="topbar">
    <button class="ghost small" onclick={() => showSettings = true}>⚙ Settings</button>
  </div>
  {#if overview}
  <div class="stats">
    <div class="stat"><div class="stat-v">{overview.online_count}<span style="color:var(--text-dim);font-weight:400">/{overview.node_count}</span></div><div class="stat-l">Nodes online</div></div>
    <div class="stat"><div class="stat-v">{overview.image_count}</div><div class="stat-l">Images</div></div>
    <div class="stat"><div class="stat-v">{overview.bridge_count ?? 0}</div><div class="stat-l">Bridges</div></div>
    <div class="stat"><div class="stat-v">{overview.connection_count ?? 0}</div><div class="stat-l">Connections</div></div>
    <div class="stat"><div class="stat-v">{overview.deployed_count ?? 0}</div><div class="stat-l">Deployed</div></div>
    <div class="stat"><div class="stat-v">{overview.container_count}</div><div class="stat-l">Containers</div></div>
    <div class="stat"><div class="stat-v">{overview.pool_free ?? '-'}/{overview.pool_total ?? '-'}</div><div class="stat-l">Ports free</div></div>
  </div>
  {/if}

  <!-- Connections -->
  <div class="section">
    <div class="section-head"><h2>Connections</h2><button class="primary" onclick={createConnection}>+ New Connection</button></div>
    {#if connections.length === 0}
      <div class="dim" style="text-align:center;padding:32px">No connections yet. Create a connection, assign a Bridge, Image, and Node — it will auto-start when all three are ready.</div>
    {:else}
    <table>
      <thead><tr><th>Name</th><th>Bridge</th><th>Image</th><th>Node</th><th>Status</th><th class="actions"></th></tr></thead>
      <tbody>
        {#each connections as c}
          <tr>
            <td class="hi">{c.name} <span class="dim" style="font-size:10px">#{c.id}</span></td>
            <td>
              <select class="slot-select" value={c.bridge_id ?? ''} onchange={(e) => { const v = e.currentTarget.value; updateConnection(c.id, 'bridge_id', v ? parseInt(v) : null); }}>
                <option value="">—</option>
                {#each bridges as b}
                  <option value={b.id} selected={c.bridge_id === b.id}>{b.name}</option>
                {/each}
              </select>
            </td>
            <td>
              <select class="slot-select" value={c.image_id ?? ''} onchange={(e) => { const v = e.currentTarget.value; updateConnection(c.id, 'image_id', v ? parseInt(v) : null); }}>
                <option value="">—</option>
                {#each images as i}
                  <option value={i.id} selected={c.image_id === i.id}>{i.name}</option>
                {/each}
              </select>
            </td>
            <td>
              <select class="slot-select" value={c.node_id ?? ''} onchange={(e) => { const v = e.currentTarget.value; updateConnection(c.id, 'node_id', v ? parseInt(v) : null); }}>
                <option value="">—</option>
                {#each online as n}
                  <option value={n.id} selected={c.node_id === n.id}>{n.hostname}</option>
                {/each}
              </select>
            </td>
            <td><span class="badge {c.status}">{c.status}</span></td>
            <td class="actions"><button class="ghost danger" onclick={() => deleteConnection(c.id)}>×</button></td>
          </tr>
        {/each}
      </tbody>
    </table>
    {/if}
  </div>

  <!-- Bridges -->
  <div class="section">
    <div class="section-head"><h2>Bridges</h2><button class="primary" onclick={() => showNewBridge = true}>+ New Bridge</button></div>
    {#if bridges.length === 0}
      <div class="dim" style="text-align:center;padding:20px">No bridges yet. Bridges are port configuration templates.</div>
    {:else}
    <table>
      <thead><tr><th>Name</th><th>Status</th><th class="actions">Actions</th></tr></thead>
      <tbody>
        {#each bridges as br}
          <tr>
            <td class="hi"><button class="ghost small mono" onclick={() => toggleBridge(br.id)}>{br.name}</button></td>
            <td><span class="badge loaded">{br.status}</span></td>
            <td class="actions"><button class="ghost danger" onclick={() => deleteBridge(br.id)}>×</button></td>
          </tr>
          {#if expandedBridge === br.id}
            <tr><td colspan="3">
              <BridgeDetail bridgeId={br.id} onlineNodes={online} />
            </td></tr>
          {/if}
        {/each}
      </tbody>
    </table>
    {/if}
  </div>

  <!-- Images -->
  <div class="section">
    <div class="section-head"><h2>Images</h2><button class="primary" onclick={() => showLoad = true}>+ Load Image</button></div>
    <table>
      <thead><tr><th>Name</th><th>Source</th><th>Type</th><th>Status</th></tr></thead>
      <tbody>
        {#each images as img}
          <tr><td class="hi">{img.name}</td><td class="dim">{img.source}</td><td class="dim">{img.source_type}</td><td><span class="badge loaded">{img.status}</span></td></tr>
        {:else}
          <tr><td colspan="4" class="dim" style="text-align:center;padding:20px">No images loaded. Click <em>+ Load Image</em> to pull from Docker Hub.</td></tr>
        {/each}
      </tbody>
    </table>
  </div>

  <!-- Nodes -->
  <div class="section">
    <div class="section-head"><h2>Nodes</h2><span class="dim">{online.length} online</span></div>
    <table>
      <thead><tr><th>Hostname</th><th>OS</th><th>Docker</th><th>CPU</th><th>Memory</th><th>Port Range</th><th>Status</th><th>Last Seen</th></tr></thead>
      <tbody>
        {#each nodes as n}
          <tr>
            <td class="hi">{n.hostname}</td><td class="dim">{n.os} {n.arch}</td><td class="dim">{dk(n.docker_version)}</td>
            <td class="dim">{n.cpu_cores} cores</td><td class="dim">{mem(n.memory_mb)}</td>
            <td class="dim">{n.port_range_start}–{n.port_range_end}</td>
            <td><span class="badge {n.status}">{n.status}</span></td><td class="dim">{ago(n.last_seen)}</td>
          </tr>
        {:else}
          <tr><td colspan="8" class="dim" style="text-align:center;padding:20px">No nodes connected.</td></tr>
        {/each}
      </tbody>
    </table>
  </div>

  <!-- New Bridge Modal -->
  {#if showNewBridge}
  <div class="overlay" onclick={() => showNewBridge = false} onkeydown={(e) => { if (e.key === 'Escape') showNewBridge = false; }}>
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
      <div class="modal-head"><span>New <em>Bridge</em></span><button class="ghost" onclick={() => showNewBridge = false}>Close</button></div>
      <div class="field"><input bind:value={newBridgeName} placeholder="Bridge name" /></div>
      <button class="primary" onclick={createBridge} disabled={!newBridgeName.trim()}>Create</button>
    </div>
  </div>
  {/if}

  <!-- Settings Modal -->
  {#if showSettings}
  <div class="overlay" onclick={() => showSettings = false} onkeydown={(e) => { if (e.key === 'Escape') showSettings = false; }}>
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
      <div class="modal-head"><span>Server <em>Settings</em></span><button class="ghost" onclick={() => showSettings = false}>Close</button></div>
      {#if settings}
        <div class="setting-row">
          <div>
            <div class="setting-title">TLS / HTTPS</div>
            <div class="dim" style="font-size:11px">
              Public reverse proxy is currently {settings.live_tls_enabled ? `serving HTTPS on :${settings.https_port}` : `serving HTTP on :${settings.http_port}`}.
              {#if !settings.tls_configurable}
                <br>To enable TLS, set <code>tls_cert_path</code> and <code>tls_key_path</code> in <code>server.toml</code>.
              {/if}
            </div>
          </div>
          <label class="switch">
            <input type="checkbox" checked={settings.tls_enabled} disabled={!settings.tls_configurable && !settings.tls_enabled} onchange={(e) => toggleTls(e.currentTarget.checked)} />
            <span class="slider"></span>
          </label>
        </div>
        {#if settings.restart_required}
          <div class="msg warn">Restart <code>kdcts</code> to apply: persisted TLS = {settings.tls_enabled}, live = {settings.live_tls_enabled}.</div>
        {/if}
        <div class="setting-meta">
          <div><span class="dim">Public HTTP port</span><span class="mono">{settings.http_port}</span></div>
          <div><span class="dim">Public HTTPS port</span><span class="mono">{settings.https_port}</span></div>
          <div><span class="dim">Panel API port</span><span class="mono">{settings.api_port}</span></div>
          <div><span class="dim">Reserved path</span><span class="mono">/admin</span></div>
        </div>
      {:else}
        <div class="dim">Loading…</div>
      {/if}
    </div>
  </div>
  {/if}

  <LoadImageModal bind:show={showLoad} onloaded={refresh} />
</div>

<style>
  .page { padding: 24px; max-width: 1200px; margin: 0 auto; }
  .stats { display: flex; gap: 16px; margin-bottom: 24px; flex-wrap: wrap; }
  .stat { background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius); padding: 12px 16px; display: flex; flex-direction: column; gap: 2px; }
  .stat-v { font-size: 20px; font-weight: 700; color: var(--text-hi); }
  .stat-l { font-size: 10px; color: var(--text-dim); text-transform: uppercase; }
  .section { margin-bottom: 32px; }
  .section-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px; }
  .section-head h2 { margin: 0; font-size: 13px; text-transform: uppercase; letter-spacing: 1px; color: var(--text-dim); }
  .badge { font-size: 10px; padding: 1px 6px; border-radius: var(--radius); font-weight: 600; }
  .badge.loaded, .badge.draft { background: var(--surface2); color: var(--text); }
  .badge.online { background: #064e3b; color: #34d399; }
  .badge.offline { background: var(--surface2); color: var(--text-dim); }
  .badge.pending { background: #4a2e00; color: #fbbf24; }
  .badge.deployed { background: #1e3a5f; color: #60a5fa; }
  .badge.direct { background: #4a1e5f; color: #c084fc; }
  .badge.route { background: #1e3a5f; color: #60a5fa; }
  .actions { text-align: right; white-space: nowrap; }
  .slot-select { font-family: var(--mono); font-size: 11px; background: var(--bg); border: 1px solid var(--border2); color: var(--text-hi); padding: 4px 6px; border-radius: var(--radius); min-width: 100px; }
  .danger { color: var(--red) !important; }
  .small { font-size: 10px !important; padding: 3px 10px !important; }
  .topbar { display: flex; justify-content: flex-end; margin-bottom: 12px; }
  .setting-row { display: flex; align-items: center; justify-content: space-between; padding: 12px 0; border-bottom: 1px solid var(--border); gap: 16px; }
  .setting-title { font-weight: 600; color: var(--text-hi); margin-bottom: 4px; }
  .setting-meta { display: grid; grid-template-columns: 1fr auto; gap: 6px 16px; margin-top: 16px; font-size: 12px; }
  .setting-meta .mono { font-family: var(--mono); color: var(--text-hi); }
  .msg.warn { background: #4a2e00; color: #fbbf24; padding: 8px 12px; border-radius: var(--radius); margin-top: 12px; font-size: 12px; }
  .switch { position: relative; display: inline-block; width: 40px; height: 22px; flex-shrink: 0; }
  .switch input { opacity: 0; width: 0; height: 0; }
  .switch .slider { position: absolute; cursor: pointer; inset: 0; background: var(--surface2); transition: .2s; border-radius: 22px; }
  .switch .slider:before { content: ""; position: absolute; height: 16px; width: 16px; left: 3px; bottom: 3px; background: var(--text-hi); transition: .2s; border-radius: 50%; }
  .switch input:checked + .slider { background: #1e3a5f; }
  .switch input:checked + .slider:before { transform: translateX(18px); background: #60a5fa; }
  .switch input:disabled + .slider { opacity: 0.4; cursor: not-allowed; }
</style>
