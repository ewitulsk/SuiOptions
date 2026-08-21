// leaderboard service entrypoint: load config, connect + migrate the DB,
// serve the public (:9021) and internal (:9022) muxes.
package main

import (
	"context"
	"errors"
	"flag"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/server"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/service"
	lbstore "github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/config"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/db"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/obs"
)

func main() {
	cfgPath := flag.String("config", "config/config.toml", "path to the TOML config")
	flag.Parse()

	ctx := context.Background()
	shutdownTrace := obs.InitTracing(ctx, "leaderboard")
	defer shutdownTrace()

	var cfg server.Config
	if err := config.LoadTOML(*cfgPath, &cfg); err != nil {
		log.Fatalf("config: %v", err)
	}

	pool, err := db.Connect(ctx, cfg.DatabaseURL, lbstore.MigrationsFS, lbstore.MigrationsDir)
	if err != nil {
		log.Fatalf("db: %v", err)
	}
	defer pool.Close()

	svc := service.New(lbstore.New(pool))

	publicSrv := &http.Server{
		Addr:              cfg.PublicBindAddr,
		Handler:           server.NewPublic(svc),
		ReadHeaderTimeout: 10 * time.Second,
	}
	internalSrv := &http.Server{
		Addr:              cfg.InternalBindAddr,
		Handler:           server.NewInternal(svc),
		ReadHeaderTimeout: 10 * time.Second,
	}

	errCh := make(chan error, 2)
	go func() {
		log.Printf("leaderboard public listening on %s (%s)", cfg.PublicBindAddr, cfg.Environment)
		errCh <- publicSrv.ListenAndServe()
	}()
	go func() {
		log.Printf("leaderboard internal listening on %s", cfg.InternalBindAddr)
		errCh <- internalSrv.ListenAndServe()
	}()

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)

	select {
	case sig := <-stop:
		log.Printf("received %s; draining", sig)
	case err := <-errCh:
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Fatalf("http: %v", err)
		}
	}
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_ = publicSrv.Shutdown(shutdownCtx)
	_ = internalSrv.Shutdown(shutdownCtx)
}
