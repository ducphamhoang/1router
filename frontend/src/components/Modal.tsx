import { ReactNode, useEffect } from "react";

type ModalProps = {
  /** Accessible name of the dialog. */
  label: string;
  onClose: () => void;
  children: ReactNode;
};

/**
 * Deliberately a plain div overlay rather than <dialog>/showModal(): jsdom's
 * <dialog> support is partial, and a modal we can't drive in Vitest is worse
 * than one we can.
 */
export function Modal({ label, onClose, children }: ModalProps) {
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") {
        onClose();
      }
    }
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label={label}
        className="modal"
        // Clicks inside the panel must not bubble up to the overlay's close.
        onClick={(event) => event.stopPropagation()}
      >
        <button type="button" className="modal-close btn-ghost" onClick={onClose} aria-label="Close dialog">
          <span aria-hidden="true">×</span>
        </button>
        {children}
      </div>
    </div>
  );
}
