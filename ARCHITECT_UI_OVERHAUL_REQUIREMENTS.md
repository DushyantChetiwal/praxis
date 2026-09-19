# Architect Application Shell Requirements

Status: Approved mockup, awaiting implementation
Mockup: [`docs/architect-ui-overhaul.html`](docs/architect-ui-overhaul.html)
Product branch: `Enhanced_Agents`
Last reviewed: 2026-09-19

## Delivery commitment

All twelve phases and every final acceptance requirement are in scope. Phase
boundaries are implementation and review milestones, not optional releases or
places to stop. The overhaul is complete only when the full checklist is done
and the GitHub-built artifact passes installation and update verification.

## How to use this checklist

- Leave an item unchecked until its implementation and focused tests pass in
  GitHub Actions.
- Mark an item complete in the same commit that completes it, or in the
  immediately following checklist-only commit after the workflow succeeds.
- Do not mark an item complete based only on the HTML mockup.
- If implementation changes a locked decision below, update this document and
  the mockup before continuing.
- A phase is complete only when every requirement and its phase exit gate are
  checked.
- Rust builds, tests, Clippy, formatting, and diagnostics run only in GitHub
  Actions. Do not run local Rust tooling.

## Evaluation of the mockup

The mockup establishes a coherent product direction:

- Architect is a primary application surface rather than a modal flowchart.
- Code is an alternate workspace, not a reduced editor embedded in Architect.
- Plan structure, step details, conversations, and execution have distinct
  regions and states.
- The Project and Git viewers share one left dock as alternate tabs.
- The overall plan conversation remains available in Code.
- Native application chrome remains visible in both workspaces.
- Primary actions have one owner. There is one plan conversation action, one
  run action, one stop action, and one canvas fit action.

The mockup is illustrative rather than a literal implementation specification:

- Zed's existing menu bar, title bar, status bar, project panel, Git panel,
  editor panes, terminal panel, and agent panel must be reused. Architect must
  not duplicate them in `agent_ui`.
- Exact colors must come from Zed theme tokens. Hard-coded mockup colors are not
  product requirements.
- Layout dimensions are defaults and constraints, not fixed pixels at every
  window size.
- Plan health, run progress, changed-file counts, and status labels must be
  derived from live state. Mockup values are examples only.
- Switching workspaces must preserve live entities. It must not reconstruct
  editors, terminals, conversations, or panels from display-only state.

## Current implementation gap

- `ArchitectPane::open` currently calls `Workspace::toggle_modal` in
  `crates/agent_ui/src/architect_ui.rs`.
- `ArchitectPane` currently renders as a near-full-window bare modal in
  `crates/agent_ui/src/architect_ui/rendering.rs`.
- An `Item` implementation exists but is intentionally dormant.
- Graph editing, nested plans, node and edge selection, step conversations,
  minimap behavior, and run orchestration already exist and should be preserved.
- `ProjectPanel`, `GitPanel`, `TerminalPanel`, editor panes, `AgentPanel`, and
  workspace chrome already exist elsewhere in Zed and should remain the source
  of truth for Code.

## Locked product decisions

- [ ] **AUI-001** Architect and Code are peer workspace modes within the same
      Zed window.
- [ ] **AUI-002** Architect is the initial mode for a first-time
      Architect-enabled workspace. Later openings restore the last selected
      mode.
- [ ] **AUI-003** A workspace without an active plan still opens Architect and
      shows an actionable empty state. It does not silently switch to Code.
- [ ] **AUI-004** Code uses Zed's native editor workspace and native panels. No
      replacement project tree, Git client, editor, terminal, or menu bar is
      built for Architect.
- [ ] **AUI-005** Project and Git are alternate native tabs in the left dock.
      They are not vertically split and are not simultaneously visible.
- [ ] **AUI-006** The right side of Code shows the overall conversation that
      owns the plan. Step-specific conversations remain in Architect.
- [ ] **AUI-007** Existing plan data, graph mutation rules, nested-plan limits,
      step locking, and run-engine behavior remain compatible.
- [ ] **AUI-008** Workspace switching never ends an active run, terminal
      process, editor session, or conversation.
- [ ] **AUI-009** Native Zed window chrome remains visible in both modes.
- [ ] **AUI-010** Stable Zed installation data and settings are not modified by
      this implementation.

## Phase 1: Workspace ownership and application shell

- [ ] **AUI-101** Introduce an explicit workspace mode state with `Architect`
      and `Code` variants.
- [ ] **AUI-102** Move Architect ownership from `Workspace::toggle_modal` to a
      workspace-owned surface that participates in normal focus and action
      routing.
- [ ] **AUI-103** Remove Architect's `ModalView` presentation and modal
      background fade.
- [ ] **AUI-104** Remove modal-only sizing, rounded dialog framing, and modal
      dismissal behavior.
- [ ] **AUI-105** Keep one Architect surface per workspace and update it when
      the owning plan thread changes.
- [ ] **AUI-106** Ensure an Architect surface cannot retain a stale thread after
      that thread closes, changes, or becomes unavailable.
- [ ] **AUI-107** Update every current `ArchitectPane::open` call site to use the
      workspace-mode API.
- [ ] **AUI-108** Preserve the existing `OpenArchitect` action and make it
      activate Architect mode instead of toggling a modal.
- [ ] **AUI-109** Add an action that activates Code mode.
- [ ] **AUI-110** Add a toggle action for keyboard and command-palette access to
      the two modes.
- [ ] **AUI-111** Route focus to the last focused control within the destination
      mode after a switch.
- [ ] **AUI-112** Keep Zed's native title bar, menus, window controls, and status
      bar outside mode-specific rendering.
- [ ] **AUI-113** Update the native window title and status-bar context for the
      active mode without replacing the native components.
- [ ] **AUI-114** Remove dead modal compatibility code after all launch paths use
      the workspace-owned surface.
- [ ] **AUI-115** Add focused tests for opening, closing, toggling, and replacing
      the active Architect thread.
- [ ] **Phase 1 exit gate:** Architect opens as a non-modal primary surface and
      Code can be restored without losing the active workspace.

## Phase 2: Workspace switching and persistence

- [ ] **AUI-201** Persist the selected Architect or Code mode with workspace
      state.
- [ ] **AUI-202** Default to Architect only when no persisted mode exists.
- [ ] **AUI-203** Preserve all open Code panes, tabs, split layout, active item,
      cursor positions, selections, and scroll positions while Architect is
      active.
- [ ] **AUI-204** Preserve running terminal entities and the active terminal tab
      while Architect is active.
- [ ] **AUI-205** Preserve Project and Git panel selection, scroll position,
      active repository, and dock width across mode switches.
- [ ] **AUI-206** Preserve the overall plan conversation, selected step, focused
      nested plan, inspector tab, canvas pan, and zoom across mode switches.
- [ ] **AUI-207** Restore the last focused control independently for Architect
      and Code.
- [ ] **AUI-208** Switching modes is immediate and does not serialize and rebuild
      live UI entities.
- [ ] **AUI-209** Switching modes during a run preserves progress and shows the
      current run state immediately on return.
- [ ] **AUI-210** Closing and reopening the application restores the mode and
      dock layout without corrupting older workspace state.
- [ ] **AUI-211** Unknown or obsolete persisted Architect state falls back to a
      safe default and reports actionable errors where appropriate.
- [ ] **AUI-212** Add migration coverage for workspace state created before the
      two-mode shell existed.
- [ ] **Phase 2 exit gate:** Repeated switching and application restart preserve
      both workspaces and all live user state.

## Phase 3: Native Code workspace

- [ ] **AUI-301** Code mode displays the existing Zed editor pane group without
      wrapping it in a mock or Architect-specific editor component.
- [ ] **AUI-302** Code mode uses the native `ProjectPanel` in the left dock.
- [ ] **AUI-303** Code mode uses the native `GitPanel` in the same left dock.
- [ ] **AUI-304** Project and Git appear as alternate dock tabs and only one is
      visible at a time.
- [ ] **AUI-305** Switching Project and Git does not resize the editor or reset
      either panel's internal state.
- [ ] **AUI-306** Code mode uses the native `TerminalPanel` in the bottom dock.
- [ ] **AUI-307** Code mode uses `AgentPanel` on the right for the overall plan
      conversation.
- [ ] **AUI-308** The right conversation remains bound to the root conversation
      that owns the active plan.
- [ ] **AUI-309** A step conversation cannot replace the overall conversation in
      Code.
- [ ] **AUI-310** Code mode respects user-configured dock sizes where they do not
      conflict with the required panel placement.
- [ ] **AUI-311** Entering Code applies the required Project/Git left dock,
      Terminal bottom dock, and plan conversation right dock without rewriting
      global user settings.
- [ ] **AUI-312** Leaving Code stores its transient dock state for the current
      workspace only.
- [ ] **AUI-313** Standard editor, Git, project, terminal, menu, and status-bar
      actions behave exactly as they do outside Architect.
- [ ] **AUI-314** Add integration coverage proving editor buffers and terminal
      entities survive an Architect-to-Code-to-Architect round trip.
- [ ] **Phase 3 exit gate:** Code is the normal Zed development workspace with
      Project/Git tabs and the overall plan conversation added to its layout.

## Phase 4: Architect workspace shell

- [ ] **AUI-401** Render Architect as three coordinated regions: plan outline,
      graph workspace, and contextual inspector.
- [ ] **AUI-402** Use a default outline width near 226 px with a usable bounded
      resize range.
- [ ] **AUI-403** Use a default inspector width near 348 px with a usable bounded
      resize range.
- [ ] **AUI-404** Persist outline and inspector widths per workspace.
- [ ] **AUI-405** Add a plan header with title, readiness or run status, step
      summary, and last-update context.
- [ ] **AUI-406** Keep workspace actions in the plan header: Open Code, Share,
      Review plan, and the single Run or Stop action.
- [ ] **AUI-407** Do not add a Graph/Outline toggle because both are already
      visible in Architect.
- [ ] **AUI-408** Add a separate canvas command bar for breadcrumbs, Search,
      Tidy, and Add step.
- [ ] **AUI-409** Keep Fit only in the canvas zoom controls.
- [ ] **AUI-410** Keep one persistent Open plan conversation action in the
      outline region.
- [ ] **AUI-411** Do not duplicate Open plan conversation, Run plan, Stop run, or
      Fit in the inspector.
- [ ] **AUI-412** Use Zed theme colors, typography, spacing tokens, borders, and
      elevation instead of hard-coded mockup values.
- [ ] **AUI-413** Use deliberate spacing between semantic groups and avoid
      compressing unrelated controls into one toolbar.
- [ ] **AUI-414** Provide an empty Architect state for no thread, no plan, and an
      empty plan, with distinct actionable messages.
- [ ] **AUI-415** Preserve existing notices and surface failures through normal
      workspace notifications.
- [ ] **Phase 4 exit gate:** Architect matches the approved information
      architecture and contains no duplicate primary actions.

## Phase 5: Plan outline and readiness

- [ ] **AUI-501** Display plan health above the step list.
- [ ] **AUI-502** Derive settled, draft, running, queued, failed, and completed
      counts from the live graph and run state.
- [ ] **AUI-503** Define a deterministic readiness calculation and test it. Do not
      display an unexplained or estimated percentage.
- [ ] **AUI-504** Display validation failures, incomplete handoffs, unlocked
      steps, and unreachable final paths as actionable readiness issues.
- [ ] **AUI-505** Selecting a readiness issue focuses the relevant step, edge, or
      plan-level problem.
- [ ] **AUI-506** Display every step in semantic execution order without relying
      on graph coordinates.
- [ ] **AUI-507** Show step number, title, responsibility, and current state in
      each outline row.
- [ ] **AUI-508** Keep outline, graph, and inspector selection synchronized in
      both directions.
- [ ] **AUI-509** Show nested-plan location and allow navigation back through the
      current `NodePath`.
- [ ] **AUI-510** Preserve the selected outline step when switching modes and
      when returning from a nested plan.
- [ ] **AUI-511** Provide keyboard navigation through outline rows and activation
      without a mouse.
- [ ] **AUI-512** Ensure state is communicated with text or icons in addition to
      color.
- [ ] **AUI-513** Add unit and GPUI coverage for readiness calculations,
      selection synchronization, and nested navigation.
- [ ] **Phase 5 exit gate:** The outline can be used to understand and navigate
      the complete plan without reading graph geometry.

## Phase 6: Graph canvas and node redesign

- [ ] **AUI-601** Redesign nodes to prioritize step number, title,
      responsibility, state, concise goal, and limited metadata.
- [ ] **AUI-602** Move full goals, rules, handoffs, sub-plan controls, and
      conversations out of node cards and into the inspector.
- [ ] **AUI-603** Keep settled, selected, running, queued, failed, and disabled
      node states visually distinct.
- [ ] **AUI-604** Preserve node dragging, grid snapping, panning, zooming,
      connection creation, edge selection, and edge hit testing.
- [ ] **AUI-605** Preserve loop-edge routing, arrow direction, edge labels, and
      edge conditions.
- [ ] **AUI-606** Preserve minimap behavior and ensure its viewport reflects the
      resized center canvas.
- [ ] **AUI-607** Preserve nested-plan drill-in and inline sub-plan expansion.
- [ ] **AUI-608** Keep compact rendering below the existing detail zoom
      threshold or replace it with an equivalent tested rule.
- [ ] **AUI-609** Update node dimensions and
      `architect::layout::{COLUMN_SPACING, ROW_SPACING}` together.
- [ ] **AUI-610** Ensure auto-layout leaves enough room for labels and does not
      overlap nodes at supported zoom levels.
- [ ] **AUI-611** Ensure zoom-to-fit accounts for the outline, inspector, run bar,
      minimap, and responsive drawer state.
- [ ] **AUI-612** Keep exactly one Fit action with a clear keyboard-accessible
      label and tooltip.
- [ ] **AUI-613** Keep Search, Tidy, and Add step in the canvas command bar and
      define their disabled states during execution.
- [ ] **AUI-614** Add geometry and layout tests for the final node dimensions,
      forward edges, loops, nested plans, and fit bounds.
- [ ] **AUI-615** Add visual coverage for small, branching, cyclic, nested, and
      large plans.
- [ ] **Phase 6 exit gate:** Existing graph capabilities work with the new cards,
      spacing, and three-region viewport.

## Phase 7: Context inspector and conversations

- [ ] **AUI-701** Show plan overview content when no step or edge is selected.
- [ ] **AUI-702** Show step Details, Conversation, and Activity as distinct
      inspector concerns.
- [ ] **AUI-703** Keep sub-plan creation and navigation available from step
      Details without crowding the node card.
- [ ] **AUI-704** Display and edit title, goal, constraints, capture rules, and
      handoff information from Details.
- [ ] **AUI-705** Display receives and captures context as a structured handoff,
      not as an unlabeled text block.
- [ ] **AUI-706** Preserve read-only behavior for locked steps and make the lock
      state obvious before editing.
- [ ] **AUI-707** Keep one Lock or Unlock step action in the step inspector.
- [ ] **AUI-708** Preserve edge-condition inspection and editing when an edge is
      selected.
- [ ] **AUI-709** Keep step conversations scoped to their step and inherited
      root-plan context.
- [ ] **AUI-710** Keep the overall plan conversation scoped to the root thread.
- [ ] **AUI-711** Opening a step conversation does not navigate `AgentPanel` away
      from the root plan conversation.
- [ ] **AUI-712** Keep exactly one Open plan conversation action in Architect.
- [ ] **AUI-713** Conversation headers clearly identify overall-plan versus
      selected-step scope.
- [ ] **AUI-714** Composer context labels accurately describe which plan,
      project, editor selection, and terminal output are included.
- [ ] **AUI-715** Sending, cancellation, loading, and error states are visible and
      do not discard drafted input.
- [ ] **AUI-716** Activity shows relevant plan mutations, lock changes,
      conversation decisions, run transitions, and validation results.
- [ ] **AUI-717** Changing selection safely rebuilds or rebinds inspector editors
      without writing content to the wrong node.
- [ ] **AUI-718** Add regression coverage for step chat creation, existing chat
      restoration, locked editors, sub-plans, and rapid selection changes.
- [ ] **Phase 7 exit gate:** Plan-level and step-level work remain clearly scoped
      and all existing inspector editing capabilities are preserved.

## Phase 8: Execution mode

- [ ] **AUI-801** Keep run orchestration in the existing agent and thread layer;
      UI components only request actions and present state.
- [ ] **AUI-802** Keep one Run plan action in the plan header.
- [ ] **AUI-803** Disable Run plan with an actionable explanation when readiness
      requirements fail.
- [ ] **AUI-804** Show a starting state immediately while run startup is pending.
- [ ] **AUI-805** Replace Run plan with one Stop run action while execution is
      active.
- [ ] **AUI-806** Do not show a second Stop run action in the inspector.
- [ ] **AUI-807** Display a workspace-wide run bar with current step, total steps,
      elapsed time, and progress derived from live state.
- [ ] **AUI-808** Reflect execution state consistently in the plan header,
      outline, graph nodes, edges, and inspector.
- [ ] **AUI-809** Show a run timeline with completed, active, waiting, queued,
      failed, cancelled, and skipped states as applicable.
- [ ] **AUI-810** Show live output with its source identified and provide one Open
      workflow action when a remote workflow exists.
- [ ] **AUI-811** Preserve current visit-safe, idempotent completion behavior and
      context handoffs between steps.
- [ ] **AUI-812** Stop requests are idempotent, report failures, and cannot leave
      the UI permanently in a stopping state.
- [ ] **AUI-813** Completion, cancellation, and failure each produce a stable
      final state that survives workspace switching.
- [ ] **AUI-814** Editing and structural graph actions have explicit enabled or
      disabled behavior during execution.
- [ ] **AUI-815** Add run-state transition coverage for start, startup failure,
      progress, stop, stop failure, cancellation, failure, and success.
- [ ] **Phase 8 exit gate:** A complete run can be understood and controlled from
      Architect with one unambiguous primary action.

## Phase 9: Responsive layout, focus, and accessibility

- [ ] **AUI-901** At wide widths, show outline, graph, and inspector
      simultaneously without clipping primary controls.
- [ ] **AUI-902** At medium widths, reduce bounded side-region widths before
      hiding content.
- [ ] **AUI-903** At narrow widths, present the inspector as a drawer over the
      graph rather than compressing the graph below usability.
- [ ] **AUI-904** At compact widths, collapse or hide the outline behind an
      explicit plan-navigation action.
- [ ] **AUI-905** Collapse secondary canvas actions before hiding primary actions.
- [ ] **AUI-906** Code mode follows native Zed responsive and panel behavior.
- [ ] **AUI-907** All controls have stable focus order, keyboard activation, and
      visible focus indication.
- [ ] **AUI-908** Mode switching, plan navigation, inspector tabs, Project/Git
      tabs, Run, Stop, Fit, and conversation actions are keyboard accessible.
- [ ] **AUI-909** Escape closes transient drawers and menus but does not exit
      Architect mode or destroy work.
- [ ] **AUI-910** Focus returns to the invoking control when a drawer or transient
      surface closes.
- [ ] **AUI-911** Icon-only controls have accessible labels and tooltips.
- [ ] **AUI-912** Text and interactive controls meet Zed's supported contrast and
      minimum hit-area conventions.
- [ ] **AUI-913** Meaning is never communicated by color alone.
- [ ] **AUI-914** Motion respects reduced-motion preferences.
- [ ] **AUI-915** Long plan names, step titles, branch names, file names, and
      localized labels truncate or wrap without overlapping controls.
- [ ] **AUI-916** Add visual coverage at wide, medium, narrow, and compact window
      sizes.
- [ ] **Phase 9 exit gate:** Both modes remain understandable and operable with
      keyboard-only input across supported window sizes.

## Phase 10: Reliability, compatibility, and performance

- [ ] **AUI-1001** Existing serialized Architect graphs load without migration
      loss.
- [ ] **AUI-1002** Existing provider aliases, thread state, step chats, nested
      plans, positions, locks, and run summaries remain compatible.
- [ ] **AUI-1003** Missing or deleted project resources do not prevent Architect
      from opening the plan.
- [ ] **AUI-1004** Missing, closed, or incompatible plan threads produce an
      actionable empty or error state instead of a panic.
- [ ] **AUI-1005** Failed entity upgrades and workspace updates are handled and
      surfaced rather than silently discarded.
- [ ] **AUI-1006** Repeated mode switches do not leak subscriptions, panels,
      editors, terminals, or thread views.
- [ ] **AUI-1007** Rapid step selection cannot trigger nested entity updates or
      write inspector content to the wrong step.
- [ ] **AUI-1008** Large plans remain interactive while panning, zooming,
      selecting, and updating run state.
- [ ] **AUI-1009** Rendering avoids rebuilding native Code components while they
      are hidden.
- [ ] **AUI-1010** Architect state changes call `cx.notify()` at the owning
      entity boundary and avoid unnecessary workspace-wide invalidation.
- [ ] **AUI-1011** Errors from asynchronous work reach the UI with enough context
      to act on them.
- [ ] **AUI-1012** No new production `unwrap`, unchecked indexing, or silently
      discarded fallible results are introduced.
- [ ] **AUI-1013** Stable Zed and Zed Dev data directories remain untouched by
      installation, state migration, and testing.
- [ ] **Phase 10 exit gate:** Compatibility tests pass and repeated use does not
      lose state, leak entities, or panic.

## Phase 11: Test and review requirements

- [ ] **AUI-1101** Preserve and update existing Architect graph, canvas,
      inspector, nested-plan, and run tests.
- [ ] **AUI-1102** Add unit coverage for workspace-mode state and persistence.
- [ ] **AUI-1103** Add GPUI integration coverage for switching between Architect
      and Code.
- [ ] **AUI-1104** Add integration coverage for native Project/Git alternate
      tabs in Code.
- [ ] **AUI-1105** Add integration coverage for preserving editors, panes,
      terminals, panels, conversations, and focus across switches.
- [ ] **AUI-1106** Add integration coverage for Architect empty, overview, step,
      edge, conversation, nested-plan, running, failed, and completed states.
- [ ] **AUI-1107** Add structural coverage that Architect exposes only one plan
      conversation action, one Run or Stop action, and one Fit action.
- [ ] **AUI-1108** Add visual tests for Architect overview, selected step, step
      conversation, running plan, and responsive drawers.
- [ ] **AUI-1109** Add visual tests for Code with Project active and Git active.
- [ ] **AUI-1110** Review focus handling, action routing, workspace persistence,
      run cancellation, and entity lifetimes for panic and data-loss risks.
- [ ] **AUI-1111** Review the final implementation against the HTML mockup and
      record any intentional deviations in this document.
- [ ] **AUI-1112** Review user-visible copy for clear scope, actionable errors,
      and consistent Architect terminology.
- [ ] **Phase 11 exit gate:** Automated coverage represents every major state in
      the approved mockup and every critical preservation guarantee.

## Phase 12: GitHub-only validation and delivery

- [ ] **AUI-1201** Keep implementation commits focused by phase and avoid mixing
      unrelated fixes.
- [ ] **AUI-1202** Run `git diff --check` locally before each push.
- [ ] **AUI-1203** Do not run local Cargo, Rust compilation, Rust tests, Clippy,
      rustfmt, Rust Analyzer, or Rust diagnostics.
- [ ] **AUI-1204** Push implementation only to `Enhanced_Agents`.
- [ ] **AUI-1205** Keep `main` limited to workflow files if workflow changes are
      required.
- [ ] **AUI-1206** Pass repository formatting checks in GitHub Actions.
- [ ] **AUI-1207** Pass focused Architect, workspace, project panel, Git panel,
      terminal panel, and agent panel tests in GitHub Actions.
- [ ] **AUI-1208** Pass the full Architect quality workflow in GitHub Actions.
- [ ] **AUI-1209** Pass Clippy in GitHub Actions without suppressing meaningful
      diagnostics.
- [ ] **AUI-1210** Confirm the successful workflow head SHA matches the source
      SHA intended for release.
- [ ] **AUI-1211** Do not trigger bundle or release workflows until all required
      quality checks are green.
- [ ] **AUI-1212** Build distributable artifacts only in GitHub Actions.
- [ ] **AUI-1213** Keep unsigned-artifact status and provenance explicit.
- [ ] **AUI-1214** Verify update installation preserves user settings, workspace
      state, and both Zed data directories.
- [ ] **AUI-1215** Update this checklist with final workflow links and the release
      source SHA.
- [ ] **Phase 12 exit gate:** The exact release source SHA is green, packaged by
      GitHub Actions, installable, and update-safe.

## Final acceptance

- [ ] **AUI-F01** Architect is the default primary surface and is not a modal.
- [ ] **AUI-F02** Code is a complete native Zed workspace reachable without
      losing Architect state.
- [ ] **AUI-F03** Project and Git are alternate left-dock tabs in Code.
- [ ] **AUI-F04** The terminal remains in the bottom dock and the overall plan
      conversation remains on the right in Code.
- [ ] **AUI-F05** Architect presents a persistent plan outline, structure-first
      graph, contextual inspector, and distinct execution mode.
- [ ] **AUI-F06** Every primary action has one clear owner and no same-scope
      duplicate.
- [ ] **AUI-F07** Existing Architect plans, conversations, nested plans, graph
      interactions, and execution behavior remain compatible.
- [ ] **AUI-F08** Responsive, keyboard, focus, error, and persistence
      requirements are satisfied.
- [ ] **AUI-F09** Required automated and visual coverage is green in GitHub
      Actions.
- [ ] **AUI-F10** The GitHub-built artifact installs and updates without losing
      user data.
