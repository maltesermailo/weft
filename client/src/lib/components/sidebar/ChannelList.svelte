<script lang="ts">
  import { getApp } from "$lib/context";
  import { voice, voiceRosters, joinVoice } from "$lib/voice.svelte";
  import VoiceBar from "$lib/components/VoiceBar.svelte";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  const rosterOf = (name: string) => Object.values(voiceRosters[name] ?? {});
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
          class:active={ch.voice ? voice.channel === ch.name : ch.name === app.active}
          class:unread={app.unreadMap[ch.name] && !app.isMuted(ch.name)}
          class:mention={app.mentionMap[ch.name]}
          class:muted={app.isMuted(ch.name)}
          class:drop-before={dt?.name === ch.name && !dt?.after}
          class:drop-after={dt?.name === ch.name && dt?.after}
          draggable="true"
          ondragstart={(e) => { app.draggingChan = ch.name; e.dataTransfer?.setData("text/plain", ch.name); if (e.dataTransfer) e.dataTransfer.effectAllowed = "move"; }}
          ondragend={() => { app.draggingChan = null; app.dropTarget = null; }}
          ondragover={(e) => { if (!app.draggingChan || app.draggingChan === ch.name) return; e.preventDefault(); const r = e.currentTarget.getBoundingClientRect(); app.dropTarget = { name: ch.name, after: e.clientY > r.top + r.height / 2 }; }}
          ondrop={(e) => { e.preventDefault(); e.stopPropagation(); if (app.draggingChan) app.moveChannel(app.draggingChan, ch.category || "Channels", ch.name, app.dropTarget?.after ?? false); app.draggingChan = null; app.dropTarget = null; }}
          onclick={() => (ch.voice ? joinVoice(ch.name) : app.open(ch.name))}
          oncontextmenu={(e) => app.chanCtx(e, ch)}>
          {#if ch.voice}
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" aria-label="voice channel"><path d="M11 5 6 9H2v6h4l5 4V5zM15.5 8.5a5 5 0 0 1 0 7M19 5a9 9 0 0 1 0 14" /></svg>
          {:else}
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" /></svg>
          {/if}
          <span class="ci-name">{app.chanShort(ch.name)}</span>
          {#if app.mentionCount[ch.name]}<span class="mention-badge">{app.mentionCount[ch.name]}</span>{/if}
          {#if app.isMuted(ch.name)}<span class="mute-icon" title="Muted" aria-label="Muted">🔕</span>{/if}
          <span class="dot {meta.cls} chan-ret" title={meta.label}></span>
        </button>
        {#if ch.voice && voice.channel === ch.name}
          <div class="voice-inline"><VoiceBar channel={ch.name} /></div>
        {:else if ch.voice && rosterOf(ch.name).length}
          <ul class="vc-roster">
            {#each rosterOf(ch.name) as p (p.user)}
              <li class="vc-member" class:speaking={p.speaking}>
                <span class="vc-avatar"><Avatar account={p.user} /></span>
                <span class="vc-name">{p.user.split("@")[0]}</span>
                {#if p.muted}<span class="vc-flag" title="Muted" aria-hidden="true">🔇</span>{/if}
              </li>
            {/each}
          </ul>
        {/if}
      {/each}
    </div>
  {/each}
  {#if !app.channelGroups.length}
    <div class="empty-hint">No channels yet.<br />Join one below.</div>
  {/if}
</div>

<style>
  /* Live "who's in voice" roster shown under a voice channel you haven't joined. */
  .vc-roster {
    list-style: none;
    margin: 1px 0 4px 22px;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .vc-member {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 2px 4px;
    border-radius: 5px;
    font-size: 0.78rem;
    color: var(--text-dim, rgba(255, 255, 255, 0.55));
  }
  .vc-avatar {
    width: 18px;
    height: 18px;
    border-radius: 50%;
    display: grid;
    place-items: center;
    font-size: 0.5rem;
    font-weight: 700;
    background: var(--bg-4, rgba(255, 255, 255, 0.1));
    outline: 2px solid transparent;
    transition: outline-color 0.1s;
  }
  .vc-member.speaking .vc-avatar {
    outline-color: #43b581;
  }
  .vc-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .vc-flag {
    font-size: 0.62rem;
    opacity: 0.7;
  }
</style>
