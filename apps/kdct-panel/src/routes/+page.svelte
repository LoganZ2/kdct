<script lang="ts">
  import { onMount } from 'svelte';

  interface ImageNode { id: number; name: string; source: string; source_type: string; status: string; created_at: number; }
  interface ImageDetail extends ImageNode {
    ports: { id: number; port: number; protocol: string; route_path: string | null }[];
    envs: { key: string; value: string }[];
    deployable: boolean;
    deploy_error: string | null;
  }
  interface NodeItem { id: number; hostname: string; os: string; arch: string; docker_version: string; port_range_start: number; port_range_end: number; cpu_cores: number; memory_mb: number; status: string; last_seen: number; }
  interface Container { container_name: string; image: string; hostname: string; ports: number[]; status: string; }
  interface Overview { node_count: number; online_count: number; image_count: number; configured_count: number; container_count: number; }

  let overview = $state<Overview | null>(null);
  let images = $state<ImageNode[]>([]);
  let nodes = $state<NodeItem[]>([]);
  let containers = $state<Container[]>([]);
  let err = $state('');

  // Expand image detail
  let expanded = $state<string | null>(null);
  let detail = $state<ImageDetail | null>(null);
  let detailLoading = $state(false);

  // Config: route editing
  let routeImg = $state('');
  let routePort = $state(0);
  let routePath = $state('');
  let routeMsg = $state('');
  let routing = $state(false);

  // Config: env editing
  let envImg = $state('');
  let envKey = $state('');
  let envVal = $state('');
  let envMsg = $state('');
  let enving = $state(false);

  // Deploy
  let deployImg = $state<string | null>(null);
  let deployNid = $state(0);
  let deploying = $state(false);
  let deployMsg = $state('');

  // Load image
  let showLoad = $state(false);
  let loading = $state(false);
  let loadJobId = $state('');
  let loadLogs = $state<string[]>([]);
  let loadResult = $state<'ok'|'err'|''>('');
  let loadMsg = $state('');
  let searchQuery = $state('');
  let searchResults = $state<any[]>([]);
  let searching = $state(false);
  let searchTimer = 0;
  let selectedRepo = $state('');
  let selectedTag = $state('');
  let tags = $state<string[]>([]);
  let tagPage = $state(1);
  let hasMoreTags = $state(false);
  let loadingTags = $state(false);
  let tagFilter = $state('');
  let customName = $state('');

  // Manual load
  let showManualModal = $state(false);
  let manualSource = $state('');
  let manualName = $state('');
  let manualLoading = $state(false);
  let manualResult = $state<'ok'|'err'|''>('');
  let manualMsg = $state('');

  const filteredTags = $derived(tagFilter.trim()
    ? tags.filter(t => t.toLowerCase().includes(tagFilter.trim().toLowerCase()))
    : tags
  );
  const online = $derived(nodes.filter(n => n.status === 'online'));

  async function refresh() {
    try {
      const [o, im, nd] = await Promise.all([
        fetch('/api/overview').then(r => r.json()),
        fetch('/api/images').then(r => r.json()),
        fetch('/api/nodes').then(r => r.json())
      ]);
      overview = o; images = im; nodes = nd; err = '';
      try { containers = await fetch('/api/deployments').then(r => r.json()); } catch { containers = []; }
    } catch { err = 'Cannot reach kdcts server'; }
  }

  onMount(() => {
    refresh();
    const iv = setInterval(refresh, 4000);
    return () => clearInterval(iv);
  });

  function mem(mb: number) { return mb >= 1024 ? `${(mb/1024).toFixed(1)} GB` : `${mb} MB`; }
  function dk(v: string) { return v ? v.split('.').slice(0,2).join('.') : '—'; }
  function ago(ts: number) {
    const s = Math.floor(Date.now()/1000 - ts);
    if (s < 60) return 'just now';
    if (s < 3600) return `${Math.floor(s/60)}m ago`;
    if (s < 86400) return `${Math.floor(s/3600)}h ago`;
    return `${Math.floor(s/86400)}d ago`;
  }
  function deps(name: string) { return containers.filter(c => c.image === name || c.image.startsWith(name + ':')); }
  function nid(h: string) { return nodes.find(n => n.hostname === h)?.id || 0; }

  // Image detail
  async function toggleDetail(name: string) {
    if (expanded === name) { expanded = null; detail = null; return; }
    expanded = name; detail = null; detailLoading = true;
    try { detail = await fetch(`/api/images/${encodeURIComponent(name)}`).then(r => r.json()); }
    catch { detail = null; }
    detailLoading = false;
  }

  async function refreshDetail() {
    if (!expanded) return;
    try { detail = await fetch(`/api/images/${encodeURIComponent(expanded)}`).then(r => r.json()); }
    catch { detail = null; }
  }

  // Route config
  function openRoute(img: string, port: number) {
    routeImg = img; routePort = port; routePath = ''; routeMsg = '';
  }
  async function doRoute() {
    if (!routePath.startsWith('/')) { routeMsg = 'Path must start with /'; return; }
    routing = true; routeMsg = '';
    try {
      const res = await fetch('/api/image/route', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ image: routeImg, port: routePort, path: routePath })
      });
      const t = await res.text();
      routeMsg = res.ok ? t : `Error: ${t}`;
      if (res.ok) { routePath = ''; refreshDetail(); }
    } catch (e: any) { routeMsg = e.message || 'Failed'; }
    routing = false;
  }

  // Env config
  function openEnv(img: string) { envImg = img; envKey = ''; envVal = ''; envMsg = ''; }
  async function doEnv() {
    if (!envKey.trim() || !envVal.trim()) { envMsg = 'Key and value required'; return; }
    enving = true; envMsg = '';
    try {
      const cur = detail?.envs || [];
      const pairs = [...cur, { key: envKey.trim(), value: envVal.trim() }];
      const res = await fetch('/api/image/env', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ image: envImg, envs: pairs })
      });
      const t = await res.text();
      envMsg = res.ok ? t : `Error: ${t}`;
      if (res.ok) { envKey = ''; envVal = ''; refreshDetail(); }
    } catch (e: any) { envMsg = e.message || 'Failed'; }
    enving = false;
  }

  async function deleteEnv(img: string, key: string) {
    const cur = detail?.envs || [];
    const pairs = cur.filter(e => e.key !== key).map(e => ({ key: e.key, value: e.value }));
    enving = true;
    try {
      const res = await fetch('/api/image/env', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ image: img, envs: pairs })
      });
      if (res.ok) refreshDetail();
    } catch {}
    enving = false;
  }

  // Deploy
  function openDeploy(img: string) {
    deployImg = img; deployNid = online[0]?.id || 0; deployMsg = '';
  }
  async function doDeploy() {
    if (!deployImg || !deployNid) return;
    deploying = true; deployMsg = '';
    try {
      const res = await fetch('/api/deploy', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ image: deployImg, node_id: deployNid })
      });
      const t = await res.text();
      if (res.ok) { deployMsg = t; deployImg = null; refresh(); }
      else deployMsg = `Error: ${t}`;
    } catch (e: any) { deployMsg = e.message || 'Deploy failed'; }
    deploying = false;
  }

  async function doStop(name: string, hostname: string) {
    const id = nid(hostname); if (!id) return;
    try {
      await fetch('/api/stop', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ image: name, node_id: id }) });
      refresh();
    } catch {}
  }

  // Load image
  function openLoad() { showLoad = true; loadResult = ''; loadMsg = ''; loadLogs = []; loadJobId = ''; searchQuery = ''; searchResults = []; selectedRepo = ''; selectedTag = ''; tags = []; tagPage = 1; hasMoreTags = false; tagFilter = ''; customName = ''; }
  function closeLoad() { if (!loading) { showLoad = false; loadJobId = ''; } }
  function handleSearchInput() {
    clearTimeout(searchTimer);
    if (searchQuery.length < 2) { searchResults = []; return; }
    searching = true;
    searchTimer = setTimeout(async () => {
      try { searchResults = await fetch(`/api/search?q=${encodeURIComponent(searchQuery)}`).then(r => r.json()); } catch { searchResults = []; }
      searching = false;
    }, 250);
  }
  async function pickImage(repo: string) {
    selectedRepo = repo; selectedTag = ''; customName = repo; searchQuery = ''; searchResults = []; tags = []; tagPage = 1; hasMoreTags = false; tagFilter = ''; loadingTags = true;
    try {
      const res = await fetch(`/api/tags?repo=${encodeURIComponent(repo)}&page=1`);
      const data = await res.json();
      tags = data.tags || []; hasMoreTags = data.has_next;
    } catch { tags = []; }
    loadingTags = false;
  }
  async function loadMoreTags() {
    if (!hasMoreTags || loadingTags) return;
    loadingTags = true; const nextPage = tagPage + 1;
    try {
      const res = await fetch(`/api/tags?repo=${encodeURIComponent(selectedRepo)}&page=${nextPage}`);
      const data = await res.json();
      tags = [...tags, ...(data.tags || [])]; hasMoreTags = data.has_next; tagPage = nextPage;
    } catch {}
    loadingTags = false;
  }
  function onTagScroll(e: Event) {
    const el = e.target as HTMLElement;
    if (el.scrollTop + el.clientHeight >= el.scrollHeight - 10) loadMoreTags();
  }
  async function doLoad() {
    if (!selectedRepo) return;
    const source = selectedTag ? `${selectedRepo}:${selectedTag}` : selectedRepo;
    const name = customName.trim(); if (!name) return;
    loading = true; loadResult = ''; loadMsg = ''; loadLogs = []; loadJobId = '';
    try {
      const res = await fetch('/api/image/load', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ source, name }) });
      const data = await res.json();
      if (data.job_id) { loadJobId = data.job_id; pollLoadProgress(); }
      else { loadResult = 'err'; loadMsg = 'Failed to start job'; loading = false; }
    } catch (e: any) { loadResult = 'err'; loadMsg = e.message || 'Load failed'; loading = false; }
  }
  async function pollLoadProgress() {
    if (!loadJobId) return;
    try {
      const res = await fetch(`/api/image/load/progress?job=${loadJobId}`);
      const data = await res.json(); loadLogs = [...(data.logs || [])];
      if (data.status === 'done') { loadResult = 'ok'; loadMsg = data.result; loading = false; refresh(); }
      else if (data.status === 'error') { loadResult = 'err'; loadMsg = data.result || 'Unknown error'; loading = false; }
      else setTimeout(pollLoadProgress, 500);
    } catch (e: any) { loadResult = 'err'; loadMsg = e.message; loading = false; }
  }

  function openManual() { showManualModal = true; manualSource = ''; manualName = ''; manualResult = ''; manualMsg = ''; loadLogs = []; loadJobId = ''; }
  async function doManualLoad() {
    const source = manualSource.trim(); if (!source) return;
    const name = manualName.trim() || source.replace(/[:/@]/g, '-');
    manualLoading = true; manualResult = ''; manualMsg = '';
    try {
      const res = await fetch('/api/image/load', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ source, name }) });
      const data = await res.json();
      if (data.job_id) { loadJobId = data.job_id; pollManualProgress(); }
      else { manualResult = 'err'; manualMsg = 'Failed to start job'; manualLoading = false; }
    } catch (e: any) { manualResult = 'err'; manualMsg = e.message || 'Load failed'; manualLoading = false; }
  }
  async function pollManualProgress() {
    if (!loadJobId) return;
    try {
      const res = await fetch(`/api/image/load/progress?job=${loadJobId}`);
      const data = await res.json(); loadLogs = [...(data.logs || [])];
      if (data.status === 'done') { manualResult = 'ok'; manualMsg = data.result; manualLoading = false; refresh(); }
      else if (data.status === 'error') { manualResult = 'err'; manualMsg = data.result || 'Unknown error'; manualLoading = false; }
      else setTimeout(pollManualProgress, 500);
    } catch (e: any) { manualResult = 'err'; manualMsg = e.message; manualLoading = false; }
  }
</script>

{#if err}
  <div class="msg err">{err}</div>
{/if}

{#if overview}
  <div class="stats">
    <div class="stat">
      <div class="stat-v">{overview.online_count}<span style="color:var(--text-dim);font-weight:400">/{overview.node_count}</span></div>
      <div class="stat-l">Nodes online</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.configured_count}<span style="color:var(--text-dim);font-weight:400">/{overview.image_count}</span></div>
      <div class="stat-l">Images configured</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.container_count}</div>
      <div class="stat-l">Containers running</div>
    </div>
  </div>
{/if}

<!-- IMAGES -->
<div class="section">
  <div class="section-head">
    <div style="display:flex;align-items:center;gap:12px">
      <h2>Images</h2>
      <span>{images.length} total</span>
    </div>
    <button class="primary" style="font-size:11px;padding:4px 10px" onclick={openLoad}>+ Load Image</button>
  </div>

  {#if images.length === 0}
    <div class="empty">No images loaded. Click <em>+ Load Image</em> to load a Docker image or Git repository.</div>
  {:else}
    <table>
      <thead><tr><th>Name</th><th>Source</th><th>Type</th><th>Status</th><th>Running on</th><th></th></tr></thead>
      <tbody>
        {#each images as img}
          {@const dd = deps(img.name)}
          <tr class="row-link" class:expanded={expanded === img.name}>
            <td class="hi" style="font-weight:500;cursor:pointer" onclick={() => toggleDetail(img.name)} onkeydown={(e) => { if (e.key === 'Enter') toggleDetail(img.name); }} role="button" tabindex="0">{img.name}</td>
            <td class="dim truncate" style="max-width:200px">{img.source}</td>
            <td class="dim">{img.source_type}</td>
            <td><span class="badge {img.status}">{img.status}</span></td>
            <td>
              {#if dd.length > 0}
                {#each dd as c}
                  <div style="display:flex;align-items:center;gap:5px;padding:1px 0">
                    <span class="dot online" style="width:5px;height:5px"></span>
                    <span class="dim" style="font-size:11px">{c.hostname}</span>
                    <span class="dim" style="font-size:11px">&middot; :{c.ports?.join(',') || '—'}</span>
                  </div>
                {/each}
              {:else}
                <span class="dim">—</span>
              {/if}
            </td>
            <td>
              <div style="display:flex;gap:6px">
                {#each dd as c}
                  <button class="danger" onclick={() => doStop(img.name, c.hostname)}>Stop</button>
                {/each}
              </div>
            </td>
          </tr>

          {#if expanded === img.name}
            <tr>
              <td colspan="6" style="padding:0;border-bottom:1px solid var(--border2)">
                <div class="expand-pad">
                  {#if detailLoading}
                    <div class="dim" style="font-size:12px;padding:16px 0">Loading...</div>
                  {:else if detail}
                    <!-- Ports -->
                    <div class="section-head" style="margin-bottom:8px"><h2>Ports & Routes</h2></div>
                    {#if detail.ports?.length}
                      <table style="margin-bottom:16px">
                        <thead><tr><th>Port</th><th>Proto</th><th>Route Path</th><th></th></tr></thead>
                        <tbody>
                          {#each detail.ports as p}
                            <tr>
                              <td class="hi">{p.port}</td>
                              <td class="dim">{p.protocol}</td>
                              <td class="dim">
                                {#if p.route_path}
                                  {p.route_path}
                                {:else}
                                  <span style="color:var(--amber)">(not set)</span>
                                {/if}
                              </td>
                              <td>
                                <button class="ghost" style="font-size:10px;padding:2px 8px" onclick={() => openRoute(img.name, p.port)}>{p.route_path ? 'Edit' : 'Set Route'}</button>
                              </td>
                            </tr>
                          {/each}
                        </tbody>
                      </table>

                      {#if routeImg === img.name && routePort > 0}
                        <div class="config-row">
                          <span class="dim" style="font-size:11px">Port :{routePort}</span>
                          <input style="width:auto;flex:1" bind:value={routePath} placeholder="/api/my-app" disabled={routing} onkeydown={(e) => { if (e.key === 'Enter') doRoute(); }} />
                          <button onclick={doRoute} disabled={routing}>{routing ? '...' : 'Save'}</button>
                        </div>
                        {#if routeMsg}
                          <div class="dim" style="font-size:10px;margin-top:4px;color:{routeMsg.startsWith('Error') ? 'var(--red)' : 'var(--green)'}">{routeMsg}</div>
                        {/if}
                      {/if}
                    {:else}
                      <div class="dim mono" style="font-size:12px;padding:8px 0">No ports configured</div>
                    {/if}

                    <!-- Envs -->
                    <div class="section-head" style="margin-bottom:8px;margin-top:16px"><h2>Environment</h2></div>
                    {#if detail.envs?.length}
                      <table style="margin-bottom:16px">
                        <thead><tr><th>Key</th><th>Value</th><th></th></tr></thead>
                        <tbody>
                          {#each detail.envs as e}
                            <tr>
                              <td class="hi">{e.key}</td>
                              <td class="dim">{e.value}</td>
                              <td><button class="ghost danger" style="font-size:10px;padding:2px 6px" onclick={() => deleteEnv(img.name, e.key)}>×</button></td>
                            </tr>
                          {/each}
                        </tbody>
                      </table>
                    {/if}

                    <div style="margin-bottom:12px">
                      <button class="ghost" style="font-size:10px" onclick={() => openEnv(img.name)}>+ Add Env</button>
                    </div>

                    {#if envImg === img.name}
                      <div class="config-row">
                        <input style="width:auto;flex:1" bind:value={envKey} placeholder="KEY" disabled={enving} />
                        <input style="width:auto;flex:2" bind:value={envVal} placeholder="VALUE" disabled={enving} />
                        <button onclick={doEnv} disabled={enving}>{enving ? '...' : 'Add'}</button>
                      </div>
                      {#if envMsg}
                        <div class="dim" style="font-size:10px;margin-top:4px;color:{envMsg.startsWith('Error') ? 'var(--red)' : 'var(--green)'}">{envMsg}</div>
                      {/if}
                    {/if}

                    <!-- Deploy -->
                    <div class="section-head" style="margin-bottom:8px;margin-top:16px"><h2>Deploy</h2></div>
                    {#if detail.deployable}
                      {#if online.length > 0}
                        <button class="primary" onclick={() => openDeploy(img.name)}>Deploy</button>
                      {:else}
                        <span class="dim" style="font-size:11px">No online nodes available</span>
                      {/if}
                    {:else}
                      <div class="dim mono" style="font-size:11px;color:var(--amber)">{detail.deploy_error || 'Not deployable'}</div>
                    {/if}
                  {/if}
                </div>
              </td>
            </tr>
          {/if}
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<!-- NODES -->
<div class="section">
  <div class="section-head">
    <h2>Nodes</h2>
    <span>{nodes.length} total / {online.length} online</span>
  </div>

  {#if nodes.length === 0}
    <div class="empty">No nodes registered. Launch a KDCT client on your target machines.</div>
  {:else}
    <table>
      <thead><tr><th></th><th>Hostname</th><th>OS</th><th>Arch</th><th>Docker</th><th>CPU</th><th>Memory</th><th>Port Range</th><th>Last Seen</th><th>Status</th></tr></thead>
      <tbody>
        {#each nodes as n}
          <tr>
            <td><span class="dot {n.status}"></span></td>
            <td class="hi">{n.hostname}</td>
            <td class="dim">{n.os || '—'}</td>
            <td class="dim">{n.arch || '—'}</td>
            <td class="dim">{dk(n.docker_version)}</td>
            <td class="dim">{n.cpu_cores} cores</td>
            <td class="dim">{mem(n.memory_mb)}</td>
            <td class="dim">{n.port_range_start} &ndash; {n.port_range_end}</td>
            <td class="dim">{ago(n.last_seen)}</td>
            <td><span class="badge {n.status}">{n.status}</span></td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<!-- DEPLOY MODAL -->
{#if deployImg}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={() => deployImg = null} onkeydown={(e) => { if (e.key === 'Escape') deployImg = null; }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
      <div class="modal-head">
        <span>Deploy <em>{deployImg}</em></span>
        <button class="ghost" onclick={() => deployImg = null}>Cancel</button>
      </div>
      {#if deploying}
        <div class="dim" style="font-size:11px;padding:16px 0">Deploying...</div>
      {:else if deployMsg}
        <div class="msg {deployMsg.startsWith('Error') ? 'err' : 'ok'}">{deployMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => { if (!deployMsg.startsWith('Error')) deployImg = null; else deployMsg = ''; }}>
          {deployMsg.startsWith('Error') ? 'Try again' : 'Close'}
        </button>
      {:else}
        <div class="field">
          <label for="dn">Target node</label>
          <select id="dn" bind:value={deployNid}>
            {#each online as n}
              <option value={n.id}>{n.hostname} (id {n.id})</option>
            {/each}
          </select>
        </div>
        <button class="primary" onclick={doDeploy} disabled={!deployNid}>Deploy</button>
      {/if}
    </div>
  </div>
{/if}

<!-- LOAD IMAGE MODAL -->
{#if showLoad}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={closeLoad} onkeydown={(e) => { if (e.key === 'Escape') closeLoad(); }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
      <div class="modal-head">
        <span>Load <em>Image</em></span>
        <button class="ghost" onclick={closeLoad} disabled={loading}>Close</button>
      </div>
      {#if loadResult === 'ok'}
        <div class="msg ok">{loadMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => showLoad = false}>Close</button>
      {:else if loadResult === 'err'}
        <div class="msg err">{loadMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => { loadResult = ''; loadMsg = ''; }}>Try again</button>
      {:else if loading}
        <div class="field">
          <label>Image</label>
          <div class="picked-repo">{selectedRepo}{selectedTag ? `:${selectedTag}` : ''}</div>
        </div>
        <div class="log-console">
          {#each loadLogs as line}<div class="log-line">{line}</div>{/each}
        </div>
        <div class="dim" style="font-size:10px;text-align:center;margin-bottom:8px">Do not close or refresh this page</div>
      {:else if !selectedRepo}
        <div class="section-head" style="margin-bottom:10px"><h2>Docker Hub</h2></div>
        <div class="field"><input bind:value={searchQuery} oninput={handleSearchInput} placeholder="Search nginx, redis, postgres..." disabled={loading} /></div>
        {#if searching}<div class="dim" style="font-size:11px;padding:4px 0 8px">Searching...</div>{/if}
        {#if searchResults.length > 0}
          <div class="search-list">
            {#each searchResults as r}
              <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
              <div class="search-item" onclick={() => pickImage(r.name)} onkeydown={(e) => { if (e.key === 'Enter') pickImage(r.name); }} role="button" tabindex="0">
                <div><span class="hi">{r.name}</span>{#if r.is_official}<span class="badge online">OFFICIAL</span>{/if}</div>
                <div class="dim" style="font-size:10px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">{r.description || '—'}</div>
              </div>
            {/each}
          </div>
        {/if}
        <button class="ghost" style="width:100%;margin-top:12px" onclick={() => { showLoad = false; openManual(); }}>Manual entry</button>
      {:else}
        <div class="field"><label>Image</label><div class="picked-repo">{selectedRepo}</div></div>
        <div class="field">
          <label for="tag">Tag</label>
          {#if loadingTags && tags.length === 0}
            <div class="dim" style="font-size:11px">Loading tags...</div>
          {:else}
            <input style="margin-bottom:6px" bind:value={tagFilter} placeholder="Filter tags..." disabled={loading} />
            <div class="tag-list" onscroll={onTagScroll}>
              {#each filteredTags as t}
                <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
                <div class="tag-item" class:selected={selectedTag === t} onclick={() => selectedTag = selectedTag === t ? '' : t} onkeydown={(e) => { if (e.key === 'Enter') selectedTag = selectedTag === t ? '' : t; }} role="option" tabindex="0">{t}</div>
              {/each}
              {#if filteredTags.length === 0 && tagFilter.trim()}<div class="dim" style="font-size:10px;padding:6px 10px">No matching tags</div>{/if}
              {#if loadingTags}<div class="dim" style="font-size:10px;padding:6px 10px">Loading more...</div>{:else if hasMoreTags}<div class="dim" style="font-size:10px;padding:6px 10px">Scroll for more...</div>{/if}
            </div>
          {/if}
        </div>
        <div class="field"><label for="cname">Name</label><input id="cname" bind:value={customName} placeholder="My custom name" disabled={loading} /><div class="dim" style="font-size:10px;margin-top:4px">Required</div></div>
        <div style="margin-bottom:12px"><button class="ghost" onclick={() => selectedRepo = ''}>Change image</button></div>
        <button class="primary" onclick={doLoad} disabled={loading || !customName.trim()}>{loading ? 'Loading...' : 'Load Image'}</button>
      {/if}
    </div>
  </div>
{/if}

<!-- MANUAL ENTRY MODAL -->
{#if showManualModal}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={() => showManualModal = false} onkeydown={(e) => { if (e.key === 'Escape') showManualModal = false; }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
      <div class="modal-head">
        <span>Manual <em>Entry</em></span>
        <button class="ghost" onclick={() => showManualModal = false} disabled={manualLoading}>Close</button>
      </div>
      {#if manualResult === 'ok'}
        <div class="msg ok">{manualMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => showManualModal = false}>Close</button>
      {:else if manualResult === 'err'}
        <div class="msg err">{manualMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => { manualResult = ''; manualMsg = ''; }}>Try again</button>
      {:else if manualLoading}
        <div class="field"><label>Source</label><div class="picked-repo">{manualSource}</div></div>
        <div class="log-console">{#each loadLogs as line}<div class="log-line">{line}</div>{/each}</div>
        <div class="dim" style="font-size:10px;text-align:center;margin-bottom:8px">Do not close or refresh this page</div>
      {:else}
        <div class="field"><label for="msrc">Source</label><input id="msrc" bind:value={manualSource} placeholder="nginx:alpine  or  git URL" disabled={manualLoading} /><div class="dim" style="font-size:10px;margin-top:4px">Docker Hub image:tag, or Git repository URL</div></div>
        <div class="field"><label for="mname">Name</label><input id="mname" bind:value={manualName} placeholder={manualSource.replace(/[:/@]/g, '-') || 'my-image'} disabled={manualLoading} /><div class="dim" style="font-size:10px;margin-top:4px">Required</div></div>
        <button class="primary" style="width:100%" onclick={doManualLoad} disabled={manualLoading || !manualSource.trim()}>{manualLoading ? 'Loading...' : 'Load Image'}</button>
      {/if}
    </div>
  </div>
{/if}
