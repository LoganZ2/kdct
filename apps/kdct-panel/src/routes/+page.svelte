<script lang="ts">
  let overview: any = null;
  let images: any[] = [];
  let nodes: any[] = [];
  let bridges: any[] = [];
  let bridgeDetail: any = null;
  let expandedBridge: number | null = null;

  // Modals
  let showLoad = false;
  let showDeploy = false;
  let deployBridgeId = 0;
  let deployNodeId = 0;
  let deployBridgeName = '';

  // Load image
  let searchQuery = '';
  let searchResults: any[] = [];
  let searching = false;
  let pickedRepo = '';
  let loading = false;
  let loadStatus = '';
  let loadLogs: string[] = [];
  let loadJobId = '';
  let loadName = '';
  let showTags = false;
  let tags: string[] = [];
  let tagPage = 1;
  let tagHasNext = false;
  let tagFilter = '';
  let pickedTag = '';
  let showManual = false;

  // Bridge edit
  let addingPort = false;
  let portContainerPort = 0;
  let portMode = 'route';
  let portRoutePath = '';
  let portProtocols: string[] = ['tcp'];
  let addingEnv = false;
  let envKey = '';
  let envVal = '';
  let portMsg = '';
  let envMsg = '';

  // New bridge
  let showNewBridge = false;
  let newBridgeName = '';
  let newBridgeImage = '';

  // Timer
  import { onMount, onDestroy } from 'svelte';
  let timer: any = null;

  async function refresh() {
    try {
      const [ov, im, nd, br] = await Promise.all([
        fetch('/api/overview').then(r => r.json()),
        fetch('/api/images').then(r => r.json()),
        fetch('/api/nodes').then(r => r.json()),
        fetch('/api/bridges').then(r => r.json()),
      ]);
      overview = ov; images = im; nodes = nd; bridges = br;
      if (expandedBridge) { refreshBridgeDetail(expandedBridge); }
    } catch {}
  }

  async function refreshBridgeDetail(id: number) {
    try {
      bridgeDetail = await fetch(`/api/bridges/${id}`).then(r => r.json());
    } catch { bridgeDetail = null; }
  }

  function toggleBridge(id: number) {
    if (expandedBridge === id) { expandedBridge = null; bridgeDetail = null; return; }
    expandedBridge = id; bridgeDetail = null; refreshBridgeDetail(id);
  }

  onMount(() => { refresh(); timer = setInterval(refresh, 5000); });
  onDestroy(() => { if (timer) clearInterval(timer); });

  // Search Docker Hub
  async function doSearch(q: string) {
    if (q.length < 2) { searchResults = []; return; }
    searching = true;
    try {
      searchResults = await fetch(`/api/search?q=${encodeURIComponent(q)}`).then(r => r.json());
    } catch { searchResults = []; }
    searching = false;
  }

  function pickRepo(repo: string) {
    pickedRepo = repo; pickedTag = ''; tags = []; tagPage = 1; showTags = true;
    fetchTags(repo, 1);
  }

  async function fetchTags(repo: string, page: number) {
    try {
      const res = await fetch(`/api/tags?repo=${encodeURIComponent(repo)}&page=${page}`).then(r => r.json());
      if (page === 1) { tags = res.tags || []; }
      else { tags = [...tags, ...(res.tags || [])]; }
      tagHasNext = res.has_next;
      tagPage = page;
    } catch {}
  }

  function loadMoreTags() {
    if (tagHasNext) fetchTags(pickedRepo, tagPage + 1);
  }

  function filteredTags() {
    if (!tagFilter) return tags;
    return tags.filter(t => t.toLowerCase().includes(tagFilter.toLowerCase()));
  }

  async function doLoad() {
    if (!pickedRepo) return;
    loading = true; loadStatus = 'connecting'; loadLogs = [];
    try {
      const body: any = { source: pickedRepo };
      if (pickedTag) body.source = `${pickedRepo}:${pickedTag}`;
      if (loadName) body.name = loadName;
      const res = await fetch('/api/image/load', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
      const { job_id } = await res.json();
      loadJobId = job_id;
      pollLoad();
    } catch (e: any) { loadLogs = [e.message || 'Failed']; loading = false; }
  }

  function pollLoad() {
    const iv = setInterval(async () => {
      try {
        const res = await fetch(`/api/image/load/progress?job=${loadJobId}`).then(r => r.json());
        loadLogs = res.logs || [];
        loadStatus = res.status;
        if (res.status === 'done' || res.status === 'error') {
          loading = false;
          clearInterval(iv);
          refresh();
        }
      } catch {}
    }, 500);
  }

  function closeLoad() { showLoad = false; loading = false; loadLogs = []; loadStatus = ''; loadJobId = ''; pickedRepo = ''; pickedTag = ''; tags = []; showTags = false; loadName = ''; }

  // Bridge CRUD
  async function createBridge() {
    if (!newBridgeName || !newBridgeImage) return;
    try {
      await fetch('/api/bridges', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: newBridgeName, image: newBridgeImage }),
      });
      showNewBridge = false; newBridgeName = ''; newBridgeImage = '';
      refresh();
    } catch (e: any) { }
  }

  async function deleteBridge(id: number) {
    if (!confirm('Delete this bridge?')) return;
    await fetch(`/api/bridges/${id}`, { method: 'DELETE' });
    if (expandedBridge === id) { expandedBridge = null; bridgeDetail = null; }
    refresh();
  }

  async function addPort(bridgeId: number) {
    if (!portContainerPort) return;
    if (portMode === 'route' && !portRoutePath.startsWith('/')) { portMsg = 'Path must start with /'; return; }
    portMsg = '';
    try {
      const body: any = { container_port: portContainerPort, mode: portMode };
      if (portMode === 'route') body.route_path = portRoutePath;
      if (portMode === 'direct') body.protocols = portProtocols;
      const res = await fetch(`/api/bridges/${bridgeId}/port`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
      });
      if (res.ok) { portContainerPort = 0; portRoutePath = ''; addingPort = false; portProtocols = ['tcp']; refreshBridgeDetail(bridgeId); }
      else { portMsg = await res.text(); }
    } catch (e: any) { portMsg = e.message; }
  }

  async function deletePort(bridgeId: number, containerPort: number) {
    await fetch(`/api/bridges/${bridgeId}/port/${containerPort}`, { method: 'DELETE' });
    refreshBridgeDetail(bridgeId);
  }

  async function addEnv(bridgeId: number) {
    if (!envKey) return;
    const cur = bridgeDetail?.envs || [];
    const pairs = [...cur, { key: envKey, value: envVal }];
    envMsg = '';
    try {
      const res = await fetch(`/api/bridges/${bridgeId}/env`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ envs: pairs }),
      });
      if (res.ok) { envKey = ''; envVal = ''; addingEnv = false; refreshBridgeDetail(bridgeId); }
      else { envMsg = await res.text(); }
    } catch (e: any) { envMsg = e.message; }
  }

  async function deleteEnv(bridgeId: number, key: string) {
    const cur = bridgeDetail?.envs || [];
    const pairs = cur.filter((e: any) => e.key !== key);
    try {
      await fetch(`/api/bridges/${bridgeId}/env`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ envs: pairs }),
      });
      refreshBridgeDetail(bridgeId);
    } catch {}
  }

  function openDeploy(id: number, name: string) {
    deployBridgeId = id; deployBridgeName = name; deployNodeId = 0; showDeploy = true;
  }

  async function doDeploy() {
    if (!deployBridgeId || !deployNodeId) return;
    try {
      await fetch(`/api/bridges/${deployBridgeId}/deploy`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ node_id: deployNodeId }),
      });
      showDeploy = false;
      refresh();
    } catch (e: any) { }
  }

  async function doStop(bridgeId: number) {
    try {
      await fetch(`/api/bridges/${bridgeId}/stop`, { method: 'POST' });
      refresh();
    } catch (e: any) { }
  }

  $: onlineNodes = nodes.filter((n: any) => n.status === 'online');
  $: filteredImages = images;
</script>

<div class="page">
  <!-- Stats -->
  {#if overview}
  <div class="stats">
    <div class="stat"><span class="stat-val">{overview.online_count}</span> <span class="dim">nodes online</span></div>
    <div class="stat"><span class="stat-val">{overview.image_count}</span> <span class="dim">images</span></div>
    <div class="stat"><span class="stat-val">{overview.bridge_count ?? 0}</span> <span class="dim">bridges</span></div>
    <div class="stat"><span class="stat-val">{overview.deployed_count ?? 0}</span> <span class="dim">deployed</span></div>
    <div class="stat"><span class="stat-val">{overview.container_count}</span> <span class="dim">containers</span></div>
    <div class="stat"><span class="stat-val">{overview.pool_free ?? '-'}/{overview.pool_total ?? '-'}</span> <span class="dim">free/total ports</span></div>
  </div>
  {/if}

  <!-- Images -->
  <div class="section">
    <div class="section-head"><h2>Images</h2><button class="ghost small" on:click={() => showLoad = true}>+ Load Image</button></div>
    <table>
      <thead><tr><th>Name</th><th>Source</th><th>Type</th><th>Status</th><th class="actions">Actions</th></tr></thead>
      <tbody>
        {#each filteredImages as img}
          <tr>
            <td class="hi">{img.name}</td>
            <td class="dim">{img.source}</td>
            <td class="dim">{img.source_type}</td>
            <td><span class="badge {img.status}">{img.status}</span></td>
            <td class="actions">
              <button class="ghost small" on:click={() => { newBridgeImage = img.name; newBridgeName = img.name.replace(/[/:]/g, '-'); showNewBridge = true; }}>Create Bridge</button>
            </td>
          </tr>
        {:else}
          <tr><td colspan="5" class="dim" style="text-align:center;padding:32px">No images loaded. Use <b>Load Image</b> to pull from Docker Hub.</td></tr>
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
            <td class="hi"><button class="ghost small mono" on:click={() => toggleBridge(br.id)}>{br.name}</button></td>
            <td class="dim">{br.image_name}</td>
            <td><span class="badge {br.status}">{br.status}</span></td>
            <td class="dim">{br.node_id ?? '-'}</td>
            <td class="actions">
              {#if deployed}
                <button class="ghost small danger" on:click={() => doStop(br.id)}>Stop</button>
              {:else}
                <button class="ghost small" on:click={() => openDeploy(br.id, br.name)}>Deploy</button>
              {/if}
              <button class="ghost small danger" on:click={() => deleteBridge(br.id)} style="margin-left:4px">×</button>
            </td>
          </tr>
          {#if expandedBridge === br.id && bridgeDetail}
            <tr><td colspan="5">
              <div class="detail-panel">
                <!-- Ports -->
                <div class="section-head" style="margin-bottom:8px"><h3>Ports</h3></div>
                {#if bridgeDetail.ports?.length}
                  <table style="margin-bottom:8px">
                    <thead><tr><th>Container Port</th><th>Mode</th><th>Route Path</th><th>Protocols</th><th></th></tr></thead>
                    <tbody>
                      {#each bridgeDetail.ports as p}
                        <tr>
                          <td class="hi">{p.container_port}</td>
                          <td>{#if p.mode === 'direct'}<span class="badge direct">direct</span>{:else}<span class="badge route">route</span>{/if}</td>
                          <td class="dim">{p.mode === 'route' ? (p.route_path || '-') : '-'}</td>
                          <td class="dim">{p.mode === 'direct' ? (p.protocols || 'tcp') : 'http'}</td>
                          <td><button class="ghost small danger" on:click={() => deletePort(br.id, p.container_port)}>×</button></td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                {/if}
                {#if addingPort && br.id === expandedBridge}
                  <div class="config-row" style="margin-bottom:8px;flex-wrap:wrap">
                    <input type="number" bind:value={portContainerPort} placeholder="Container port" style="width:60px" />
                    <select bind:value={portMode} style="font-family:var(--mono);font-size:11px;background:var(--bg);border:1px solid var(--border2);color:var(--text-hi);padding:4px">
                      <option value="route">route</option>
                      <option value="direct">direct</option>
                    </select>
                    {#if portMode === 'route'}
                      <input bind:value={portRoutePath} placeholder="/api/..." style="flex:1" />
                    {:else}
                      <label style="font-size:10px;display:flex;align-items:center;gap:2px"><input type="checkbox" bind:group={portProtocols} value="tcp" />TCP</label>
                      <label style="font-size:10px;display:flex;align-items:center;gap:2px"><input type="checkbox" bind:group={portProtocols} value="udp" />UDP</label>
                    {/if}
                    <button class="ghost small" on:click={() => addPort(br.id)}>Add</button>
                    <button class="ghost small" on:click={() => addingPort = false}>Cancel</button>
                  </div>
                  {#if portMsg}<div class="dim" style="font-size:10px;color:var(--red)">{portMsg}</div>{/if}
                {:else}
                  <button class="ghost small" on:click={() => { addingPort = true; portContainerPort = 0; portMode = 'route'; portRoutePath = ''; portProtocols = ['tcp']; portMsg = ''; }} style="margin-bottom:8px">+ Add Port</button>
                {/if}

                <!-- Envs -->
                <div class="section-head" style="margin-bottom:8px;margin-top:16px"><h3>Environment</h3></div>
                {#if bridgeDetail.envs?.length}
                  <table style="margin-bottom:8px">
                    <thead><tr><th>Key</th><th>Value</th><th></th></tr></thead>
                    <tbody>
                      {#each bridgeDetail.envs as e}
                        <tr>
                          <td class="hi">{e.key}</td>
                          <td class="dim">{e.value}</td>
                          <td><button class="ghost small danger" on:click={() => deleteEnv(br.id, e.key)}>×</button></td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                {/if}
                {#if addingEnv && br.id === expandedBridge}
                  <div class="config-row" style="margin-bottom:8px">
                    <input bind:value={envKey} placeholder="KEY" style="flex:1" />
                    <input bind:value={envVal} placeholder="VALUE" style="flex:2" />
                    <button class="ghost small" on:click={() => addEnv(br.id)}>Add</button>
                    <button class="ghost small" on:click={() => addingEnv = false}>Cancel</button>
                  </div>
                  {#if envMsg}<div class="dim" style="font-size:10px;color:var(--red)">{envMsg}</div>{/if}
                {:else}
                  <button class="ghost small" on:click={() => { addingEnv = true; envKey = ''; envVal = ''; envMsg = ''; }}>+ Add Env</button>
                {/if}

                <!-- Deploy info -->
                {#if bridgeDetail.deployable}
                  <div class="section-head" style="margin-bottom:8px;margin-top:16px"><h3>Deploy</h3></div>
                  {#if onlineNodes.length > 0}
                    <button class="primary small" on:click={() => openDeploy(br.id, br.name)}>Deploy</button>
                  {:else}
                    <span class="dim" style="font-size:11px">No online nodes</span>
                  {/if}
                {:else if bridgeDetail.ports?.length > 0}
                  <div class="dim" style="margin-top:8px;color:var(--amber);font-size:11px">{bridgeDetail.deploy_error || 'Not deployable'}</div>
                {/if}
              </div>
            </td></tr>
          {/if}
        {/each}
      </tbody>
    </table>
    {/if}
  </div>

  <!-- Nodes -->
  <div class="section">
    <div class="section-head"><h2>Nodes</h2><span class="dim">{onlineNodes.length} online</span></div>
    <table>
      <thead><tr><th>Hostname</th><th>OS</th><th>Docker</th><th>CPU</th><th>Memory</th><th>Port Range</th><th>Status</th></tr></thead>
      <tbody>
        {#each nodes as n}
          <tr>
            <td class="hi">{n.hostname}</td>
            <td class="dim">{n.os} {n.arch}</td>
            <td class="dim">{n.docker_version}</td>
            <td class="dim">{n.cpu_cores} cores</td>
            <td class="dim">{n.memory_mb} MB</td>
            <td class="dim">{n.port_range_start}–{n.port_range_end}</td>
            <td><span class="badge {n.status}">{n.status}</span></td>
          </tr>
        {:else}
          <tr><td colspan="7" class="dim" style="text-align:center;padding:32px">No nodes connected. Start a kdct client node on another machine.</td></tr>
        {/each}
      </tbody>
    </table>
  </div>
</div>

<!-- Load Image Modal -->
{#if showLoad}
<div class="modal-overlay" on:click={() => closeLoad()}></div>
<div class="modal">
  <h2>Load Image</h2>
  <button class="ghost small" style="position:absolute;top:8px;right:8px" on:click={() => closeLoad()}>×</button>

  {#if loading}
    <div class="log-console">
      {#each loadLogs as line}
        <div class="log-line">{line}</div>
      {/each}
    </div>
    {#if loadStatus === 'done'}
      <button class="primary small" style="margin-top:8px" on:click={() => closeLoad()}>Close</button>
    {/if}
  {:else}
    {#if !pickedRepo}
      <input class="search-input" bind:value={searchQuery} on:input={() => doSearch(searchQuery)} placeholder="Search Docker Hub... (e.g., nginx, node, redis)" />
      {#if searching}<div class="dim" style="padding:8px">Searching...</div>{/if}
      <div class="search-list">
        {#each searchResults as r}
          <div class="search-item" on:click={() => pickRepo(r.name)}>
            <span class="hi">{r.name}</span>
            {#if r.is_official}<span class="badge official">official</span>{/if}
            <span class="dim">★{r.star_count} ↓{r.pull_count}</span>
          </div>
        {/each}
      </div>
      <button class="ghost small" style="margin-top:8px" on:click={() => showManual = true}>Or enter manually...</button>
    {:else}
      <div class="picked-repo">
        <b>{pickedRepo}{pickedTag ? `:${pickedTag}` : ''}</b>
        <button class="ghost small" on:click={() => { pickedRepo = ''; pickedTag = ''; showTags = false; }}>Change</button>
      </div>
      <input bind:value={loadName} placeholder="Custom name (optional)" style="margin-top:8px" />

      {#if showTags}
        <input bind:value={tagFilter} placeholder="Filter tags..." style="margin-top:8px" />
        <div class="tag-list" on:scroll={(e) => { const el = e.currentTarget; if (el.scrollTop + el.clientHeight >= el.scrollHeight - 10) loadMoreTags(); }}>
          <div class="tag-item {pickedTag === '' ? 'selected' : ''}" on:click={() => pickedTag = ''} style="font-style:italic">(latest)</div>
          {#each filteredTags() as t}
            <div class="tag-item {pickedTag === t ? 'selected' : ''}" on:click={() => pickedTag = t}>{t}</div>
          {/each}
        </div>
      {/if}

      <button class="primary" style="margin-top:12px" on:click={doLoad}>Load</button>
    {/if}
  {/if}

  {#if showManual}
    <div class="modal" style="position:fixed;top:20%;left:30%;width:400px;z-index:200">
      <h3>Enter Image Source</h3>
      <input bind:value={pickedRepo} placeholder="e.g., nginx:latest, ghcr.io/org/image" style="width:100%" />
      <div style="margin-top:8px">
        <button class="ghost small" on:click={() => { showManual = false; showTags = true; }}>OK</button>
        <button class="ghost small" on:click={() => showManual = false}>Cancel</button>
      </div>
    </div>
  {/if}
</div>
{/if}

<!-- New Bridge Modal -->
{#if showNewBridge}
<div class="modal-overlay" on:click={() => showNewBridge = false}></div>
<div class="modal">
  <h2>New Bridge</h2>
  <button class="ghost small" style="position:absolute;top:8px;right:8px" on:click={() => showNewBridge = false}>×</button>
  <div style="margin-top:8px">
    <input bind:value={newBridgeName} placeholder="Bridge name" />
    <input bind:value={newBridgeImage} placeholder="Image name" style="margin-top:8px" disabled={newBridgeImage !== ''} />
    <button class="primary small" style="margin-top:12px" on:click={createBridge}>Create</button>
  </div>
</div>
{/if}

<!-- Deploy Modal -->
{#if showDeploy}
<div class="modal-overlay" on:click={() => showDeploy = false}></div>
<div class="modal">
  <h2>Deploy: {deployBridgeName}</h2>
  <button class="ghost small" style="position:absolute;top:8px;right:8px" on:click={() => showDeploy = false}>×</button>
  <div style="margin-top:8px">
    <div class="dim" style="margin-bottom:8px">Select target node:</div>
    {#each onlineNodes as n}
      <label class="radio-row">
        <input type="radio" bind:group={deployNodeId} value={n.id} />
        {n.hostname} ({n.os}, {n.cpu_cores} cores, {n.memory_mb}MB)
      </label>
    {/each}
    <button class="primary small" style="margin-top:12px" on:click={doDeploy} disabled={!deployNodeId}>Deploy</button>
  </div>
</div>
{/if}

<style>
  .page { padding: 24px; max-width: 1200px; margin: 0 auto; }
  .stats { display: flex; gap: 16px; margin-bottom: 24px; flex-wrap: wrap; }
  .stat { background: var(--surface); border: 1px solid var(--border); border-radius: var(--radius); padding: 12px 16px; display: flex; align-items: baseline; gap: 6px; }
  .stat-val { font-size: 20px; font-weight: 700; color: var(--text-hi); }
  .section { margin-bottom: 32px; }
  .section-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px; }
  .section-head h2, .section-head h3 { margin: 0; font-size: 13px; text-transform: uppercase; letter-spacing: 1px; color: var(--text-dim); }
  .badge { font-size: 10px; padding: 1px 6px; border-radius: var(--radius); font-weight: 600; }
  .badge.loaded { background: var(--surface2); color: var(--text); }
  .badge.online { background: #064e3b; color: #34d399; }
  .badge.offline { background: var(--surface2); color: var(--text-dim); }
  .badge.draft { background: var(--surface2); color: var(--text); }
  .badge.deployed { background: #1e3a5f; color: #60a5fa; }
  .badge.direct { background: #4a1e5f; color: #c084fc; }
  .badge.route { background: #1e3a5f; color: #60a5fa; }
  .badge.official { background: #1e3a5f; color: #60a5fa; margin-left: 6px; }
  .actions { text-align: right; white-space: nowrap; }
  .detail-panel { padding: 12px 16px; background: var(--surface); border-radius: var(--radius); max-width: 600px; }
  .config-row { display: flex; align-items: center; gap: 8px; }
  .config-row input, .config-row select { font-family: var(--mono); font-size: 11px; background: var(--bg); border: 1px solid var(--border2); color: var(--text-hi); padding: 5px 8px; border-radius: var(--radius); outline: none; }
  .config-row input:focus { border-color: #52525b; }
  .radio-row { display: flex; align-items: center; gap: 8px; padding: 6px 0; cursor: pointer; font-size: 12px; }
  .danger { color: var(--red) !important; }
  .small { font-size: 10px !important; padding: 3px 10px !important; }
</style>
