export function Toast({ message }: { message: string }) {
  return (
    <div className="toast">
      <div className="toast__dot"></div>
      {message}
    </div>
  );
}
