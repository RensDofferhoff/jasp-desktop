#!/usr/bin/env Rscript
# Diagnostic: (1) is the `batched` "not identical" just a row.names artifact?
# (2) decompose read_batched timing -- why isn't it ~Arrow's 11ms floor?
suppressMessages(library(arrow))
cat("=== diagnostic ===\n\n")
t <- function(f, reps = 5) { for (i in 1) {invisible(f()); gc(FALSE)}; ts <- numeric(reps)
  for (i in seq_len(reps)) { gc(FALSE); t0 <- Sys.time(); invisible(f()); ts[i] <- as.numeric(Sys.time()-t0,"secs")*1000 }; stats::median(ts) }
label_or_value <- function(values, labels) { if (is.null(labels)||length(labels)==0L) return(values)
  vapply(values, function(v){l<-labels[[v]]; if(!is.null(l)&&nzchar(l)) unname(l) else v}, character(1)) }
get_labels <- function(field){ meta<-field$metadata; if(!is.null(meta[["jasp:labels"]])&&nzchar(meta[["jasp:labels"]])) jsonlite::fromJSON(meta[["jasp:labels"]]) else NULL }
finish_factor <- function(col, as, labels){ if(as=="scale") return(as.numeric(levels(col))[as.integer(col)])
  levels(col) <- label_or_value(levels(col), labels); if(as=="ordinal"&&!is.ordered(col)) class(col)<-c("ordered","factor"); col }

read_naive <- function(path, spec){ tbl<-read_feather(path,as_data_frame=FALSE); n<-tbl$num_rows; out<-vector("list",length(spec))
  for(i in seq_along(spec)){ s<-spec[[i]]; field<-tbl$schema$GetFieldByName(s$name); col<-tbl[[s$name]]
    f<-col$as_vector(); lab<-label_or_value(levels(f),get_labels(field))
    out[[i]]<-switch(s$as, ordinal=ordered(factor(f,levels=levels(f),labels=lab)), nominal=factor(f,levels=levels(f),labels=lab), scale=as.numeric(levels(f))[as.integer(f)]) }
  names(out)<-vapply(spec,function(s)s$name,character(1)); structure(out,class="data.frame",row.names=c(NA_integer_,n)) }
read_batched <- function(path, spec){ sch<-read_feather(path,as_data_frame=FALSE)$schema; df<-read_feather(path,as_data_frame=TRUE)
  for(s in spec){ col<-df[[s$name]]; if(is.factor(col)) df[[s$name]]<-finish_factor(col,s$as,get_labels(sch$GetFieldByName(s$name))) }
  df }

gen <- function(nrows, ncols, nlevels, dir){ set.seed(42); lvl<-as.character(seq_len(nlevels))
  labs<-setNames(paste0("Lab",seq_len(nlevels)),lvl); lj<-jsonlite::toJSON(as.list(labs),auto_unbox=TRUE)
  cols<-lapply(seq_len(ncols),function(j) ordered(factor(sample(seq_len(nlevels),nrows,replace=TRUE),levels=seq_len(nlevels))))
  nm<-paste0("ord",seq_len(ncols)); names(cols)<-nm; dt<-dictionary(index_type=int32(),value_type=utf8(),ordered=TRUE)
  fields<-lapply(nm,function(n) field(n,dt,metadata=list("jasp:display_name"=n,"jasp:labels"=lj)))
  tbl<-do.call(Table$create,c(cols,list(schema=do.call(schema,fields))))
  path<-file.path(dir,sprintf("ord_%d_%d.arrow",nrows,nlevels)); write_feather(tbl,path,compression="lz4")
  list(path=path, spec=lapply(nm,function(n) list(name=n,as="ordinal"))) }

dir<-file.path(tempdir(),"jasp_diag"); dir.create(dir,showWarnings=FALSE)

# (1) correctness: compare ignoring attributes, and per-column
g<-gen(1e4,6,9,dir); a<-read_naive(g$path,g$spec); b<-read_batched(g$path,g$spec)
cat("--- correctness of batched vs naive ---\n")
cat("  identical():                          ", identical(a,b), "\n")
cat("  all.equal(check.attributes=FALSE):    ", isTRUE(all.equal(a,b,check.attributes=FALSE)), "\n")
cat("  every column identical (ignoring df): ", all(vapply(names(a), function(nm) identical(a[[nm]], b[[nm]]), logical(1))), "\n")
cat("  row.names naive  :", paste(utils::head(attr(a,"row.names"),3),collapse=","), "...\n")
cat("  row.names batched:", paste(utils::head(attr(b,"row.names"),3),collapse=","), "...\n\n")

# (2) timing decomposition on 1M x 10 ordinal
g2<-gen(1e6,10,50,dir); path<-g2$path; spec<-g2$spec
sch<-read_feather(path,as_data_frame=FALSE)$schema
df0<-read_feather(path,as_data_frame=TRUE)
t_schema <- t(function() read_feather(path,as_data_frame=FALSE)$schema)
t_data   <- t(function() read_feather(path,as_data_frame=TRUE))
t_overlay<- t(function(){ df<-df0; for(s in spec){col<-df[[s$name]]; if(is.factor(col)) df[[s$name]]<-finish_factor(col,s$as,get_labels(sch$GetFieldByName(s$name)))}; df })
t_both   <- t(function() read_batched(path,spec))
cat("--- timing decomposition (ordinal 1M x 10, 50 lv; median ms) ---\n")
cat(sprintf("  schema read only        : %7.2f\n", t_schema))
cat(sprintf("  data read only (TRUE)   : %7.2f\n", t_data))
cat(sprintf("  overlay only (in-mem)   : %7.2f\n", t_overlay))
cat(sprintf("  schema+data+overlay     : %7.2f  (read_batched)\n", t_both))
cat(sprintf("  --> data+overlay would be ~%.1f ms; the schema read adds ~%.1f ms\n", t_data+t_overlay, t_both-(t_data+t_overlay)))
unlink(dir,recursive=TRUE); cat("\nDone.\n")
