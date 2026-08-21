// event-ingestor entrypoint: load config, connect + migrate the DB, start
// the forward poller supervisor and the backfill worker, serve the admin
// (:9023) and internal (:9024) muxes.
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

	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/backfill"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/lbclient"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/poller"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/server"
	ingstore "github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/authclient"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/config"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/db"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/obs"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suigraphql"
)

func main() {
	cfgPath := flag.String("config", "config/config.toml", "path to the TOML config")
	flag.Parse()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	shutdownTrace := obs.InitTracing(ctx, "event-ingestor")
	defer shutdownTrace()

	var cfg server.Config
	if err := config.LoadTOML(*cfgPath, &cfg); err != nil {
		log.Fatalf("config: %v", err)
	}

	pool, err := db.Connect(ctx, cfg.DatabaseURL, ingstore.MigrationsFS, ingstore.MigrationsDir)
	if err != nil {
		log.Fatalf("db: %v", err)
	}
	defer pool.Close()

	st := ingstore.New(pool)
	gql := suigraphql.New(cfg.SuiGraphqlURL)
	lb := lbclient.New(cfg.LeaderboardURL)
	auth := authclient.New(cfg.AuthServiceURL)

	go poller.New(poller.Config{
		PollIntervalMs: cfg.PollIntervalMs,
		RetryBaseMs:    cfg.RetryBaseMs,
		RetryCapMs:     cfg.RetryCapMs,
	}, gql, st, lb).Run(ctx)
	go backfill.New(gql, st, lb, cfg.BackfillPagesPerSec).Run(ctx)

	adminSrv := &http.Server{
		Addr:              cfg.AdminBindAddr,
		Handler:           server.NewAdmin(st, gql, auth),
		ReadHeaderTimeout: 10 * time.Second,
	}
	internalSrv := &http.Server{
		Addr:              cfg.InternalBindAddr,
		Handler:           server.NewInternal(),
		ReadHeaderTimeout: 10 * time.Second,
	}

	errCh := make(chan error, 2)
	go func() {
		log.Printf("event-ingestor admin listening on %s (%s)", cfg.AdminBindAddr, cfg.Environment)
		errCh <- adminSrv.ListenAndServe()
	}()
	go func() {
		log.Printf("event-ingestor internal listening on %s", cfg.InternalBindAddr)
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
	cancel() // stop poller + backfill
	shutdownCtx, cancelShutdown := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancelShutdown()
	_ = adminSrv.Shutdown(shutdownCtx)
	_ = internalSrv.Shutdown(shutdownCtx)
}
