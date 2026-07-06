<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();
</script>

<div class="channel-scroll">
  {#each app.channelGroups as group (group.category)}
    <div
      class="retention-group"
      class:drop-target={app.draggingChan}
      role="group"
      ondragover={(e) => app.draggingChan && e.preventDefault()}
      ondrop={(e) => { e.preventDefault(); if (app.draggingChan) app.moveChannel(app.draggingChan, group.category); app.draggingChan = null; }}>
      <div class="retention-label cat-header" role="group" oncontextmenu={(e) => app.catCtx(e, group.category)}>
        <span>{group.category}</span>
        <button class="cat-add" title="Create channel" aria-label="Create channel in {group.category}" onclick={() => app.openCreateChannelInCat(group.category)}>+</button>
      </div>
      {#each group.list as ch (ch.name)}
        {@const meta = app.retentionMeta[ch.retention]}
        {@const dt = app.dropTarget}
        <button
          class="channel-item"
          class:active={ch.name === app.active}
          class:unread={app.unreadMap[ch.name]}
          class:mention={app.mentionMap[ch.name]}
          class:drop-before={dt?.name === ch.name && !dt?.after}
          class:drop-after={dt?.name === ch.name && dt?.after}
          draggable="true"
          ondragstart={(e) => { app.draggingChan = ch.name; e.dataTransfer?.setData("text/plain", ch.name); if (e.dataTransfer) e.dataTransfer.effectAllowed = "move"; }}
          ondragend={() => { app.draggingChan = null; app.dropTarget = null; }}
          ondragover={(e) => { if (!app.draggingChan || app.draggingChan === ch.name) return; e.preventDefault(); const r = e.currentTarget.getBoundingClientRect(); app.dropTarget = { name: ch.name, after: e.clientY > r.top + r.height / 2 }; }}
          ondrop={(e) => { e.preventDefault(); e.stopPropagation(); if (app.draggingChan) app.moveChannel(app.draggingChan, ch.category || "Channels", ch.name, app.dropTarget?.after ?? false); app.draggingChan = null; app.dropTarget = null; }}
          onclick={() => app.open(ch.name)}
          oncontextmenu={(e) => app.chanCtx(e, ch)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" /></svg>
          <span class="ci-name">{app.chanShort(ch.name)}</span>
          {#if app.mentionMap[ch.name]}<span class="mention-badge">@</span>{/if}
          <span class="dot {meta.cls} chan-ret" title={meta.label}></span>
        </button>
      {/each}
    </div>
  {/each}
  {#if !app.channelGroups.length}
    <div class="empty-hint">No channels yet.<br />Join one below.</div>
  {/if}
</div>
