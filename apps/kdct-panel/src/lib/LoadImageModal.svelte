<script lang="ts">
  import { base } from '$app/paths';
  let { show = $bindable(false), onclose = () => {}, onloaded = () => {} } = $props();

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

  let loading = $state(false);
  let loadJobId = $state('');
  let loadLogs = $state<string[]>([]);
  let loadResult = $state<'ok'|'err'|''>('');
  let loadMsg = $state('');

  // Manual entry
  let showManual = $state(false);
  let manualSource = $state('');
  let manualName = $state('');
  let manualLoading = $state(false);
  let manualResult = $state<'ok'|'err'|''>('');
  let manualMsg = $state('');

  const filteredTags = $derived(tagFilter.trim()
    ? tags.filter((t: string) => t.toLowerCase().includes(tagFilter.trim().toLowerCase()))
    : tags
  );

  function reset() {
    loadResult = ''; loadMsg = ''; loadLogs = []; loadJobId = '';
    searchQuery = ''; searchResults = []; selectedRepo = ''; selectedTag = '';
    tags = []; tagPage = 1; hasMoreTags = false; tagFilter = ''; customName = '';
  }

  $effect(() => { if (show) reset(); });

  function close() { if (!loading && !manualLoading) { show = false; onclose(); } }

  function handleSearch() {
    clearTimeout(searchTimer);
    if (searchQuery.length < 2) { searchResults = []; return; }
    searching = true;
    searchTimer = setTimeout(async () => {
      try { searchResults = await fetch(`${base}/api/search?q=${encodeURIComponent(searchQuery)}`).then(r => r.json()); } catch { searchResults = []; }
      searching = false;
    }, 250) as unknown as number;
  }

  async function pickImage(repo: string) {
    selectedRepo = repo; selectedTag = ''; customName = repo; searchQuery = ''; searchResults = [];
    tags = []; tagPage = 1; hasMoreTags = false; tagFilter = ''; loadingTags = true;
    try {
      const res = await fetch(`${base}/api/tags?repo=${encodeURIComponent(repo)}&page=1`);
      const data = await res.json();
      tags = data.tags || []; hasMoreTags = data.has_next;
    } catch { tags = []; }
    loadingTags = false;
  }

  async function loadMoreTags() {
    if (!hasMoreTags || loadingTags) return;
    loadingTags = true; const nextPage = tagPage + 1;
    try {
      const res = await fetch(`${base}/api/tags?repo=${encodeURIComponent(selectedRepo)}&page=${nextPage}`);
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
      const res = await fetch(`${base}/api/image/load`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ source, name }) });
      const data = await res.json();
      if (data.job_id) { loadJobId = data.job_id; pollProgress(); }
      else { loadResult = 'err'; loadMsg = 'Failed to start job'; loading = false; }
    } catch (e: any) { loadResult = 'err'; loadMsg = e.message || 'Load failed'; loading = false; }
  }

  function pollProgress() {
    if (!loadJobId) return;
    (async () => {
      try {
        const res = await fetch(`${base}/api/image/load/progress?job=${loadJobId}`);
        const data = await res.json(); loadLogs = [...(data.logs || [])];
        if (data.status === 'done') { loadResult = 'ok'; loadMsg = data.result; loading = false; onloaded(); }
        else if (data.status === 'error') { loadResult = 'err'; loadMsg = data.result || 'Unknown error'; loading = false; }
        else setTimeout(pollProgress, 500);
      } catch (e: any) { loadResult = 'err'; loadMsg = e.message; loading = false; }
    })();
  }

  function openManual() { showManual = true; manualSource = ''; manualName = ''; manualResult = ''; manualMsg = ''; loadLogs = []; loadJobId = ''; }

  async function doManualLoad() {
    const source = manualSource.trim(); if (!source) return;
    const name = manualName.trim() || source.replace(/[:/@]/g, '-');
    manualLoading = true; manualResult = ''; manualMsg = '';
    try {
      const res = await fetch(`${base}/api/image/load`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ source, name }) });
      const data = await res.json();
      if (data.job_id) { loadJobId = data.job_id; pollManualProgress(); }
      else { manualResult = 'err'; manualMsg = 'Failed to start job'; manualLoading = false; }
    } catch (e: any) { manualResult = 'err'; manualMsg = e.message || 'Load failed'; manualLoading = false; }
  }

  function pollManualProgress() {
    if (!loadJobId) return;
    (async () => {
      try {
        const res = await fetch(`${base}/api/image/load/progress?job=${loadJobId}`);
        const data = await res.json(); loadLogs = [...(data.logs || [])];
        if (data.status === 'done') { manualResult = 'ok'; manualMsg = data.result; manualLoading = false; onloaded(); }
        else if (data.status === 'error') { manualResult = 'err'; manualMsg = data.result || 'Unknown error'; manualLoading = false; }
        else setTimeout(pollManualProgress, 500);
      } catch (e: any) { manualResult = 'err'; manualMsg = e.message; manualLoading = false; }
    })();
  }
</script>

{#if show}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={close} onkeydown={(e) => { if (e.key === 'Escape') close(); }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
      <div class="modal-head">
        <span>Load <em>Image</em></span>
        <button class="ghost" onclick={close} disabled={loading}>Close</button>
      </div>

      {#if loadResult === 'ok'}
        <div class="msg ok">{loadMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => show = false}>Close</button>
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
        <div class="field"><input bind:value={searchQuery} oninput={handleSearch} placeholder="Search nginx, redis, postgres..." disabled={loading} /></div>
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
        <button class="ghost" style="width:100%;margin-top:12px" onclick={() => { show = false; openManual(); }}>Manual entry</button>
      {:else}
        <div class="field"><label>Image</label><div class="picked-repo">{selectedRepo}</div></div>
        <div class="field">
          <label for="tag">Tag</label>
          {#if loadingTags && tags.length === 0}
            <div class="dim" style="font-size:11px">Loading tags...</div>
          {:else}
            <input style="margin-bottom:6px" bind:value={tagFilter} placeholder="Filter tags..." disabled={loading} />
            <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
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

<!-- Manual entry modal -->
{#if showManual}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={() => showManual = false} onkeydown={(e) => { if (e.key === 'Escape') showManual = false; }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" style="z-index:200" onclick={(e) => e.stopPropagation()} onkeydown={(e) => e.stopPropagation()}>
      <div class="modal-head">
        <span>Manual <em>Entry</em></span>
        <button class="ghost" onclick={() => showManual = false} disabled={manualLoading}>Close</button>
      </div>
      {#if manualResult === 'ok'}
        <div class="msg ok">{manualMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => showManual = false}>Close</button>
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
