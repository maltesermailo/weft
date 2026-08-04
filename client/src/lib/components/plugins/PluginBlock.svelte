<script lang="ts">
  // One SDUI component (plugin-spec.md §10). Inputs bind straight into the open
  // view's `values`, which is what a SUBMIT sends — so there is no separate form
  // state to keep in step.
  //
  // An unrecognised `type` renders nothing. That is deliberate (§10): a block
  // from a newer server should cost that block, not the whole dialog.
  import { renderMd } from "$lib/rendering/mdrender.svelte";
  import type { Button, Component } from "$lib/plugins/sdui";

  let {
    block,
    values = $bindable(),
    disabled = false,
    onpress,
    onsubmit,
  }: {
    block: Component;
    values: Record<string, unknown>;
    disabled?: boolean;
    onpress: (button: Button) => void;
    onsubmit: () => void;
  } = $props();

  // A `confirm` button asks before it fires (§10.3). Held here rather than in
  // the store: it is a property of this control being pressed, nothing more.
  let confirming = $state<Button | null>(null);

  function press(b: Button) {
    if (b.confirm) confirming = b;
    else onpress(b);
  }
</script>

{#if block.type === "text"}
  <label class="field">
    <span>{block.label}{#if block.required}<em>*</em>{/if}</span>
    {#if block.multiline}
      <textarea
        bind:value={values[block.id]}
        placeholder={block.placeholder ?? ""}
        maxlength={block.max_len}
        {disabled}
        rows="4"
      ></textarea>
    {:else}
      <input
        type="text"
        bind:value={values[block.id]}
        placeholder={block.placeholder ?? ""}
        maxlength={block.max_len}
        pattern={block.pattern}
        {disabled}
      />
    {/if}
  </label>
{:else if block.type === "number"}
  <label class="field">
    <span>{block.label}{#if block.required}<em>*</em>{/if}</span>
    <input
      type="number"
      bind:value={values[block.id]}
      min={block.min}
      max={block.max}
      step={block.step}
      {disabled}
    />
  </label>
{:else if block.type === "select"}
  <label class="field">
    <span>{block.label}{#if block.required}<em>*</em>{/if}</span>
    <select bind:value={values[block.id]} {disabled}>
      {#each block.options as opt (opt.value)}
        <option value={opt.value}>{opt.label}</option>
      {/each}
    </select>
  </label>
{:else if block.type === "multiselect"}
  <fieldset class="field">
    <legend>{block.label}</legend>
    {#each block.options as opt (opt.value)}
      <label class="check">
        <input
          type="checkbox"
          checked={((values[block.id] as string[]) ?? []).includes(opt.value)}
          {disabled}
          onchange={(e) => {
            const on = e.currentTarget.checked;
            const current = ((values[block.id] as string[]) ?? []).filter((v) => v !== opt.value);

            values[block.id] = on ? [...current, opt.value] : current;
          }}
        />
        <span>{opt.label}</span>
      </label>
    {/each}
  </fieldset>
{:else if block.type === "toggle"}
  <label class="check">
    <input
      type="checkbox"
      checked={values[block.id] === true}
      {disabled}
      onchange={(e) => (values[block.id] = e.currentTarget.checked)}
    />
    <span>{block.label}</span>
  </label>
{:else if block.type === "date"}
  <label class="field">
    <span>{block.label}{#if block.required}<em>*</em>{/if}</span>
    <input type="date" bind:value={values[block.id]} min={block.min} max={block.max} {disabled} />
  </label>
{:else if block.type === "heading"}
  {#if (block.level ?? 2) <= 2}
    <h2 class="sdui-heading">{block.text}</h2>
  {:else}
    <h3 class="sdui-heading">{block.text}</h3>
  {/if}
{:else if block.type === "markdown"}
  <!-- Sanitized by the shared renderer: a plugin's text is untrusted input. -->
  <div class="sdui-md">{@html renderMd(block.text)}</div>
{:else if block.type === "divider"}
  <hr class="sdui-divider" />
{:else if block.type === "keyvalue"}
  <dl class="sdui-kv">
    {#each block.rows as row (row.key)}
      <dt>{row.key}</dt>
      <dd>{row.value}</dd>
    {/each}
  </dl>
{:else if block.type === "table"}
  <div class="sdui-table-wrap">
    <table class="sdui-table" class:dense={block.dense}>
      <thead>
        <tr>
          {#each block.columns as col (col)}<th>{col}</th>{/each}
        </tr>
      </thead>
      <tbody>
        {#each block.rows as row, i (i)}
          <tr>
            {#each row as cell, j (j)}<td>{cell}</td>{/each}
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{:else if block.type === "image"}
  <img
    class="sdui-image"
    src={block.src}
    alt={block.alt ?? ""}
    style={block.max_height ? `max-height:${block.max_height}px` : undefined}
  />
{:else if block.type === "button"}
  <button class="sdui-btn" class:danger-btn={block.style === "danger"} class:ok-btn={block.style === "primary"} {disabled} onclick={() => press(block)}>
    {block.label}
  </button>
{:else if block.type === "action-row"}
  <div class="sdui-row">
    {#each block.buttons as b (b.id)}
      <button class="sdui-btn" class:danger-btn={b.style === "danger"} class:ok-btn={b.style === "primary"} {disabled} onclick={() => press(b)}>
        {b.label}
      </button>
    {/each}
  </div>
{:else if block.type === "submit"}
  <div class="sdui-row end">
    <button class="ok-btn" class:danger-btn={block.style === "danger"} {disabled} onclick={onsubmit}>
      {block.label ?? "Submit"}
    </button>
  </div>
{/if}

{#if confirming}
  <div class="sdui-confirm">
    <p>{confirming.confirm}</p>
    <div class="sdui-row end">
      <button class="linkish" onclick={() => (confirming = null)}>Cancel</button>
      <button
        class="danger-btn"
        onclick={() => {
          const b = confirming;
          confirming = null;
          if (b) onpress(b);
        }}
      >
        {confirming.label}
      </button>
    </div>
  </div>
{/if}

<style>
  .field {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    margin-bottom: 0.75rem;
  }
  .field > span,
  .field legend {
    font-size: 0.82rem;
    opacity: 0.8;
  }
  .field em {
    color: var(--danger, #e5534b);
    font-style: normal;
    margin-left: 0.15rem;
  }
  .check {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 0.4rem;
  }
  .sdui-heading {
    margin: 0.5rem 0 0.35rem;
  }
  .sdui-md {
    font-size: 0.9rem;
    line-height: 1.45;
  }
  .sdui-divider {
    border: none;
    border-top: 1px solid var(--border, #33363d);
    margin: 0.75rem 0;
  }
  .sdui-kv {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.25rem 0.75rem;
    font-size: 0.88rem;
    margin: 0 0 0.75rem;
  }
  .sdui-kv dt {
    opacity: 0.7;
  }
  .sdui-kv dd {
    margin: 0;
  }
  /* Wide content scrolls inside its own box so the dialog never does. */
  .sdui-table-wrap {
    overflow-x: auto;
    margin-bottom: 0.75rem;
  }
  .sdui-table {
    border-collapse: collapse;
    width: 100%;
    font-size: 0.88rem;
  }
  .sdui-table th,
  .sdui-table td {
    text-align: left;
    padding: 0.4rem 0.55rem;
    border-bottom: 1px solid var(--border, #33363d);
  }
  .sdui-table.dense th,
  .sdui-table.dense td {
    padding: 0.2rem 0.45rem;
  }
  .sdui-image {
    max-width: 100%;
    border-radius: 6px;
  }
  .sdui-row {
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
    margin-bottom: 0.5rem;
  }
  .sdui-row.end {
    justify-content: flex-end;
  }
  .sdui-confirm {
    border: 1px solid var(--danger, #e5534b);
    border-radius: 6px;
    padding: 0.6rem 0.7rem;
    margin-top: 0.5rem;
  }
  .sdui-confirm p {
    margin: 0 0 0.5rem;
    font-size: 0.88rem;
  }
</style>
