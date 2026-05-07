<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import LoadImageModal from '$lib/LoadImageModal.svelte';
  import BridgeDetail from '$lib/BridgeDetail.svelte';

  let overview: any = $state(null);
  let images: any[] = $state([]);
  let nodes: any[] = $state([]);
  let bridges: any[] = $state([]);
  let err = $state('');

  let showLoad = $state(false);
  let showNewBridge = $state(false);
  let newBridgeName = $state('');
  let newBridgeImage = $state('');

  let expandedBridge = $state<number | null>(null);
  let bridgeDetail = $state<any>(null);

  let timer: any = null;

  async function refresh() {
    try {
      const [ov, im, nd, br] = await Promise.all([
        fetch('/api/overview').then(r => r.json()),
        fetch('/api/images').then(r => r.json()),
        fetch('/api/nodes').then(r => r.json()),
        fetch('/api/bridges').then(r => r.json()),
      ]);
      overview = ov; images = im; nodes = nd; bridges = br; err = '';
      if (expandedBridge) refreshBridgeDetail(expandedBridge);
    } catch { err = 'Cannot reach kdcts server'; }
  }

  async function refreshBridgeDetail(id: number) {
    try { bridgeDetail = await fetch(`/api/bridges/${id}`).then(r => r.json()); }
    catch { bridgeDetail = null; }
  }

  function toggleBridge(id: number) {
    if (expandedBridge === id) { expandedBridge = null; bridgeDetail = null; return; }
    expandedBridge = id; bridgeDetail = null; refreshBridgeDetail(id);
  }

  onMount(() => { refresh(); timer = setInterval(refresh, 5000); });
  onDestroy(() => { if (timer) clearInterval(timer); });

  function mem(mb: number) { return mb >= 1024 ? `${(mb/1024).toFixed(1)} GB` : `${mb} MB`; }
  function dk(v: string) { return v ? v.split('.').slice(0,2).join('.') : '—'; }
  function ago(ts: number) {
    const s = Math.floor(Date.now()/1000 - ts);
    if (s < 60) return 'just now';
    if (s < 3600) return `${Math.floor(s/60)}m ago`;
    if (s < 86400) return `${Math.floor(s/3600)}h ago`;
    return `${Math.floor(s/86400)}d ago`;
  }

  async function deleteBridge(id: number) {
    if (!confirm('Delete this bridge?')) return;
    await fetch(`/api/bridges/${id}`, { method: 'DELETE' });
    if (expandedBridge === id) { expandedBridge = null; bridgeDetail = null; }
    refresh();
  }

  async function doStop(bridgeId: number) {
    try { await fetch(`/api/bridges/${bridgeId}/stop`, { method: 'POST' }); refresh(); }
    catch {}
  }

  async function createBridge() {
    if (!newBridgeName || !newBridgeImage) return;
    try {
      await fetch('/api/bridges', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: newBridgeName, image: newBridgeImage }),
      });
      showNewBridge = false; newBridgeName = ''; newBridgeImage = '';
      refresh();
    } catch {}
  }

  const online = $derived(nodes.filter((n: any) => n.status === 'online'));
</script>

{#if err}
  <div class="msg err">{err}</div>
{/if}

<div class="page">
  <!-- Stats -->
  {#if overview}
  <div class="stats">
    <div class="stat">
      <div class="stat-v">{overview.online_count}<span style="color:var(--text-dim);font-weight:400">/{overview.node_count}</span></div>
      <div class="stat-l">Nodes online</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.image_count}</div>
      <div class="stat-l">Images</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.bridge_count ?? 0}</div>
      <div class="stat-l">Bridges</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.deployed_count ?? 0}</div>
      <div class="stat-l">Deployed</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.container_count}</div>
      <div class="stat-l">Containers</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.pool_free ?? '-'}/{overview.pool_total ?? '-'}</div>
      <div class="stat-l">Ports free</div>
    </div>
  </div>
  {/if}

  <!-- Images -->
  <div class="section">
    <div class="section-head">
      <h2>Images</h2>
      <button class="primary" style="font-size:11px;padding:4px 10px" onclick={() => showLoad = true}>+ Load Image</button>
    </div>
    <table>
      <thead><tr><th>Name</th><th>Source</th><th>Type</th><th>Status</th><th class="actions">Actions</th></tr></thead>
      <tbody>
        {#each images as img}
          <tr>
            <td class="hi">{img.name}</td>
            <td class="dim">{img.source}</td>
            <td class="dim">{img.source_type}</td>
            <td><span class="badge loaded">{img.status}</span></td>
            <td class="actions">
              <button class="ghost small" onclick={() => { newBridgeImage = img.name; newBridgeName = img.name.replace(/[/:]/g, '-'); showNewBridge = true; }}>Create Bridge</button>
            </td>
          </tr>
        {:else}
          <tr><td colspan="5" class="dim" style="text-align:center;padding:32px">No images loaded. Click <em>+ Load Image</em> to pull from Docker Hub.</td></tr>
        {/each}
      </tbody>
    </table>
  </div>

  <!-- Bridges -->
  <div class="section">
    <div class="section-head"><h2>Bridges</h2></div>
    {#if bridges.length === 0}
      <div class="dim" style="text-align:center;padding:32px">No bridges yet. Load an image, then create a bridge to configure ports and deploy.</div>
    {:else}
    <table>
      <thead><tr><th>Name</th><th>Image</th><th>Status</th><th>Node</th><th class="actions">Actions</th></tr></thead>
      <tbody>
        {#each bridges as br}
          {@const deployed = br.status === 'deployed'}
          <tr>
            <td class="hi"><button class="ghost small mono" onclick={() => toggleBridge(br.id)}>{br.name}</button></td>
            <td class="dim">{br.image_name}</td>
            <td><span class="badge {br.status}">{br.status}</span></td>
            <td class="dim">{br.node_id ?? '-'}</td>
            <td class="actions">
              {#if deployed}
                <button class="ghost small danger" onclick={() => doStop(br.id)}>Stop</button>
              {:else}
                <button class="ghost small" onclick={() => toggleBridge(br.id)}>Configure</button>
              {/if}
              <button class="ghost small danger" onclick={() => deleteBridge(br.id)} style="margin-left:4px">×</button>
            </td>
          </tr>
          {#if expandedBridge === br.id}
            <tr><td colspan="5">
              <BridgeDetail bridgeId={br.id} {bridgeDetail} onlineNodes={online} onrefresh={() => refreshBridgeDetail(br.id)} />
            </td></tr>
          {/if}
        {/each}
      </tbody>
    </table>
    {/if}
  </div>

  <!-- Nodes -->
  <div class="section">
    <div class="section-head">
      <h2>Nodes</h2>
      <span class="dim">{online.length} online</span>
    </div>
    <table>
      <thead><tr><th>Hostname</th><th>OS</th><th>Docker</th><th>CPU</th><th>Memory</th><th>Port Range</th><th>Status</th><th>Last Seen</th></tr></thead>
      <tbody>
        {#each nodes as n}
          <tr>
            <td class="hi">{n.hostname}</td>
            <td class="dim">{n.os} {n.arch}</td>
            <td class="dim">{dk(n.docker_version)}</td>
            <td class="dim">{n.cpu_cores} cores</td>
            <td class="dim">{mem(n.memory_mb)}</td>
            <td class="dim">{n.port_range_start}–{n.port_range_end}</td>
            <td><span class="badge {n.status}">{n.status}</span></td>
            <td class="dim">{ago(n.last_seen)}</td>
          </tr>
        {:else}
          <tr><td colspan="8" class="dim" style="text-align:center;padding:32px">No nodes connected. Start a kdct client node on another machine.</td></tr>
        {/each}
      </tbody>
    </table>
  </div>

  <!-- New Bridge Modal -->
  {#if showNewBridge}
  <div class="modal-overlay" onclick={() => showNewBridge = false}></div>
  <div class="modal">
    <h2>New Bridge</h2>
    <button class="ghost small" style="position:absolute;top:8px;right:8px" onclick={() => showNewBridge = false}>×</button>
    <div style="margin-top:8px">
      <input bind:value={newBridgeName} placeholder="Bridge name" />
      <input bind:value={newBridgeImage} placeholder="Image name" style="margin-top:8px" disabled />
      <button class="primary small" style="margin-top:12px" onclick={createBridge}>Create</button>
    </div>
  </div>
  {/if}

  <LoadImageModal bind:show={showLoad} onloaded={refresh} />
</div>
