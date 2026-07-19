<script lang="ts">
  // Shown in the main pane when the account belongs to no servers/channels
  // (a fresh account, or one that left everything). It routes the user to the
  // Discover modal, which owns every path out: browse public servers, join by
  // name/invite, connect a foreign namespace, or create one.
  import { getApp } from "$lib/context";
  const app = getApp();
</script>

<div class="empty-home">
  <div class="empty-home-card">
    <div class="empty-home-glyph" aria-hidden="true">🧵</div>
    <h1>Welcome to {app.network}</h1>
    <p>
      You're not in any servers or channels yet. Browse public communities, join
      one by invite, connect a namespace on another network, or start your own.
    </p>
    <div class="empty-home-actions">
      <button class="ok-btn" onclick={app.openDiscover}>Discover &amp; create servers</button>
      <button class="set-btn" onclick={app.goHome}>Open direct messages</button>
    </div>
    <p class="empty-home-hint">
      Tip: you can also join a channel directly — type <code>#name</code> in the sidebar's join box.
    </p>
  </div>
</div>
