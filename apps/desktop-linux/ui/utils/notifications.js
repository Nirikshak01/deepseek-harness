/**
 * DeepSeek Harness — Native Linux Desktop Notification Dispatcher
 * Dispatches OS notifications via Tauri IPC (notify-rust) when window is unfocused/minimized.
 */

import { store } from '../store/state.js';

/**
 * Dispatch an OS notification if user preferences permit and window is unfocused.
 * @param {Object} options
 * @param {string} options.title Notification title
 * @param {string} options.body Notification body text
 * @param {string|null} [options.targetCardId] Message card ID to focus when notification is clicked
 * @param {'approvals'|'questions'|'taskAlerts'} [options.type] Notification category
 * @returns {Promise<boolean>}
 */
export async function dispatchNotification({ title, body, targetCardId = null, type = 'taskAlerts' }) {
  try {
    const notifSettings = store.getState().settings?.general?.notifications || {};
    if (notifSettings.muted) {
      return false;
    }

    if (type === 'approvals' && notifSettings.approvals === false) {
      return false;
    }
    if (type === 'questions' && notifSettings.questions === false) {
      return false;
    }
    if (type === 'taskAlerts' && notifSettings.taskAlerts === false) {
      return false;
    }

    if (window.__TAURI__ && window.__TAURI__.core) {
      return await window.__TAURI__.core.invoke('cmd_dispatch_notification', {
        title,
        body,
        targetCardId
      });
    }
  } catch (err) {
    console.warn('dispatchNotification error:', err);
  }
  return false;
}
