export type ToastVariant = "success" | "error" | "info";

export type ToastState = { message: string; variant: ToastVariant };

export function Toast({
  message,
  variant = "success",
}: {
  message: string;
  variant?: ToastVariant;
}) {
  return (
    <div className="toast">
      <div className={`toast__dot toast__dot--${variant}`}></div>
      {message}
    </div>
  );
}
