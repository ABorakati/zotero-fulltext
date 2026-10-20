/* ZFT.confirm: Effect::Confirm (batch-size and group-library confirmations).
 *
 * Owner: js-ui. Modal question with up to three answers mapped onto proto.rs ConfirmChoice:
 *   kind 'large_batch'   n items selected, cap = inst.batch_max / inst.confirm_above:
 *                        buttons [Continue -> 'yes'] [Open access only -> 'oa_only'] [Cancel -> 'cancel']
 *   kind 'group_library' library = group name: saving institutional PDFs into a shared library may breach the licence:
 *                        buttons [Save anyway -> 'yes'] [Open access only -> 'oa_only'] [Cancel -> 'cancel']
 * Implementation: Services.prompt.confirmEx(parentWin, title, text, flags, ...) with strings from ZFT.l10n
 * (zotero-fulltext-confirm-* ids, args {n, cap, library}). parentWin = Zotero.getMainWindow(). The prompt is a
 * nested-event-loop modal, so other effects keep running; ctx.scope cancellation cannot close it, so a result that
 * arrives after cancellation is discarded (the runner has already dropped the op).
 *
 * Details:
 *  - The button position is the answer: index 0/1/2 -> yes/oa_only/cancel. Any other value (a dialog closed some other
 *    way), an unknown kind, or an exception from Services.prompt -> 'cancel'. Nothing is ever assumed on failure.
 *  - confirmEx is synchronous and spins a nested event loop. ask() therefore yields one macrotask first, so the runner
 *    finishes dispatching the other effects of the step that produced this Confirm before the modal opens.
 *  - Button flags use the documented idiom BUTTON_TITLE_IS_STRING * BUTTON_POS_n (positions are multipliers, not bits).
 *    The default button (Enter) is "Continue" for a large batch and "Open access only" for a group library, so a
 *    reflex Enter never saves publisher PDFs into a shared library.
 */
(function (global) {
  'use strict';
  const ZFT = global.ZFT;

  /** Per kind: [message id of the title, message id of the body, [button message ids by position], default position]. */
  const KINDS = {
    large_batch: {
      title: 'zotero-fulltext-confirm-large-batch-title',
      body: 'zotero-fulltext-confirm-large-batch-body',
      buttons: ['zotero-fulltext-confirm-yes', 'zotero-fulltext-confirm-oa-only', 'zotero-fulltext-confirm-cancel'],
      defaultButton: 0,
    },
    group_library: {
      title: 'zotero-fulltext-confirm-group-title',
      body: 'zotero-fulltext-confirm-group-body',
      buttons: ['zotero-fulltext-confirm-save-anyway', 'zotero-fulltext-confirm-oa-only', 'zotero-fulltext-confirm-cancel'],
      defaultButton: 1,
    },
  };
  /** Button position -> proto.rs ConfirmChoice. */
  const CHOICES = ['yes', 'oa_only', 'cancel'];

  /** Message arguments per kind (only the variables the message uses). */
  function messageArgs(kind, req) {
    return kind === 'large_batch'
      ? { n: Number(req.n) || 0, cap: Number(req.cap) || 0 }
      : { library: String(req.library == null ? '' : req.library) };
  }

  function parentWindow() {
    try {
      return (typeof Zotero !== 'undefined' && typeof Zotero.getMainWindow === 'function' && Zotero.getMainWindow()) || null;
    } catch (e) {
      return null;
    }
  }

  /** An nsIPromptService constant, or its documented value when the object (a test fake) does not carry it. */
  function constant(ps, name, fallback) {
    return typeof ps[name] === 'number' ? ps[name] : fallback;
  }

  /** confirmEx flags: three string buttons, with the default button marked. */
  function buttonFlags(ps, defaultButton) {
    const str = constant(ps, 'BUTTON_TITLE_IS_STRING', 127);
    const positions = [constant(ps, 'BUTTON_POS_0', 1), constant(ps, 'BUTTON_POS_1', 256), constant(ps, 'BUTTON_POS_2', 65536)];
    const defaults = [constant(ps, 'BUTTON_POS_0_DEFAULT', 0), constant(ps, 'BUTTON_POS_1_DEFAULT', 16777216), constant(ps, 'BUTTON_POS_2_DEFAULT', 33554432)];
    return str * positions[0] + str * positions[1] + str * positions[2] + defaults[defaultButton];
  }

  /**
   * @param {{kind: 'large_batch'|'group_library', n: number, cap: number, library: string}} req proto.rs ConfirmReq
   * @param {{scope: ZFT.util.CancelScope, win?: object}} ctx
   * @returns {Promise<{choice: 'yes'|'oa_only'|'cancel'}>} proto.rs ConfirmOk
   * @throws {ZFT.util.OpError} cancelled when ctx.scope was cancelled before or while the dialog was open
   */
  async function ask(req, ctx) {
    const scope = ctx && ctx.scope;
    if (scope) scope.throwIfCancelled();
    const spec = req && Object.prototype.hasOwnProperty.call(KINDS, req.kind) ? KINDS[req.kind] : null;
    if (!spec) {
      ZFT.util.log('warn', 'confirm: unknown kind, answering cancel');
      return { choice: 'cancel' };
    }
    await ZFT.util.idle();
    if (scope) scope.throwIfCancelled();

    let index = -1;
    try {
      const ps = Services.prompt;
      const args = messageArgs(req.kind, req);
      const parent = (ctx && ctx.win) || parentWindow();
      index = ps.confirmEx(
        parent,
        ZFT.l10n.format(spec.title, args),
        ZFT.l10n.format(spec.body, args),
        buttonFlags(ps, spec.defaultButton),
        ZFT.l10n.format(spec.buttons[0]),
        ZFT.l10n.format(spec.buttons[1]),
        ZFT.l10n.format(spec.buttons[2]),
        null,
        {}
      );
    } catch (e) {
      ZFT.util.log('error', 'confirm: prompt failed, answering cancel: ' + ZFT.util.errText(e));
      return { choice: 'cancel' };
    }
    // The runner dropped the op when the scope was cancelled meanwhile: whatever the user clicked is moot.
    if (scope) scope.throwIfCancelled();
    return { choice: index === 0 || index === 1 || index === 2 ? CHOICES[index] : 'cancel' };
  }

  ZFT.confirm = { ask };
})(this);
